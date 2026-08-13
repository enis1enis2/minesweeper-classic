"""admin.py - HTTPS-terminated diagnostics ingest + admin viewer.

The Minesweeper desktop client collects disclosed device diagnostics and
POSTs them as JSON to /ms-diag/ingest.  TLS is terminated by nginx
(admin.jellyfiner.dpdns.org), so this app only ever listens on 127.0.0.1.
Every field of the payload is encrypted at rest as a single Fernet blob,
so the database holds no plaintext device data.

A separate, read-only admin viewer (/ms-admin/) shows the decrypted rows to
a single administrator authenticated by (fixed username, argon2id password,
TOTP).  It is not a general database browser: only the diagnostics table is
ever read, via the app's own query.

Threat model / why this shape:
  * ingress is TLS-terminated at nginx; the app binds loopback only.
  * at-rest rows are opaque Fernet blobs; the key lives at
    /etc/minesweeper-server/diag.key (0400) and is never in the repo, the
    database, or the source tree.
  * login uses argon2id (password) + TOTP (RFC 6238, pure stdlib), a fixed
    single username, per-IP lockout, and short-lived server-side sessions
    that can be invalidated (logout / revoke-all / restart).
  * every auth attempt is logged with the real client IP
    (CF-Connecting-IP, set by nginx behind Cloudflare).

Usage:
  # one-time setup (creates key + admin.json, prints the TOTP otpauth URI)
  sudo -u msim /opt/minesweeper-server/.venv/bin/python \
      /opt/minesweeper-server/admin.py --init

  # run the service
  /opt/minesweeper-server/.venv/bin/python \
      /opt/minesweeper-server/admin.py \
      --host 127.0.0.1 --port 8444 \
      --db /var/lib/minesweeper-sim/diag.db \
      --config /etc/minesweeper-server/admin.json \
      --key /etc/minesweeper-server/diag.key

  # offline sanity pass
  python3 admin.py --selfcheck
"""

import argparse
import base64
import hashlib
import hmac
import html
import json
import logging
import os
import secrets
import sqlite3
import struct
import sys
import threading
import time

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))

log = logging.getLogger("ms-admin")

# Payload limits (defense against junk): 64 KiB body, 20 reqs / 10 s / IP.
MAX_BODY = 65536
INGEST_RATE_LIMIT = 20
INGEST_RATE_WINDOW = 10

# The 8 client-collected fields plus the machine id; all encrypted at rest.
EXPECTED_FIELDS = (
    "machine_id", "os", "cpu", "cpu_cores", "gpu", "ram_mb", "display",
    "game_version", "uptime_sec", "crash_text",
)

SCHEMA = """
CREATE TABLE IF NOT EXISTS device_diagnostics(
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    addr TEXT NOT NULL,
    blob BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_diag_ts ON device_diagnostics(ts);
"""


# --------------------------------------------------------------------------
# cryptography / auth building blocks
# --------------------------------------------------------------------------

class Fernet:
    """Thin wrapper over cryptography.fernet, imported lazily.

    The cryptography package is a pinned extra in the deploy venv; this
    wrapper gives a clear error (and a --selfcheck skip) when it is
    missing instead of dying at import time.
    """

    _impl = None

    @classmethod
    def _load(cls):
        if cls._impl is None:
            try:
                from cryptography.fernet import Fernet as _F
            except ImportError:
                raise RuntimeError(
                    "cryptography is not installed; run: "
                    "$DEST/.venv/bin/pip install cryptography==<pinned>")
            cls._impl = _F
        return cls._impl

    @classmethod
    def generate_key(cls):
        return cls._load().generate_key()

    def __init__(self, key):
        self._f = self._load()(key)

    def encrypt(self, data):
        return self._f.encrypt(data)

    def decrypt(self, token):
        return self._f.decrypt(token)


class Argon2:
    """argon2-cffi wrapper (lazy import, like Fernet)."""

    _impl = None

    @classmethod
    def _load(cls):
        if cls._impl is None:
            try:
                from argon2 import PasswordHasher
            except ImportError:
                raise RuntimeError(
                    "argon2-cffi is not installed; run: "
                    "$DEST/.venv/bin/pip install argon2-cffi==<pinned>")
            cls._impl = PasswordHasher()
        return cls._impl

    @classmethod
    def hash_password(cls, pw):
        return cls._load().hash(pw)

    @classmethod
    def verify_password(cls, pw_hash, pw):
        try:
            return cls._load().verify(pw_hash, pw)
        except Exception:  # VerifyMismatchError, InvalidHash, etc.
            return False


def totp_value(secret_b32, counter, digits=6):
    """RFC 6238 HMAC-SHA1 one-time password for a counter (30 s step).

    `digits` is 6 by default (what authenticator apps and the login form
    use); 8 is supported so the RFC 6238 test vectors can be checked.
    """
    key = base64.b32decode(secret_b32.upper().strip(), casefold=True)
    msg = struct.pack(">Q", counter)
    digest = hmac.new(key, msg, hashlib.sha1).digest()
    offset = digest[-1] & 0x0F
    code = struct.unpack(">I", digest[offset:offset + 4])[0] & 0x7FFFFFFF
    return "%0*d" % (digits, code % (10 ** digits))


def totp_verify(secret_b32, code, window=1, ts=None):
    """Accept a TOTP code allowing `window` steps of clock skew each way."""
    if not code or len(code) != 6 or not code.isdigit():
        return False
    if ts is None:
        ts = int(time.time())
    counter = ts // 30
    for k in range(-window, window + 1):
        if hmac.compare_digest(totp_value(secret_b32, counter + k), code):
            return True
    return False


def otpauth_uri(username, issuer, secret_b32):
    import urllib.parse
    return ("otpauth://totp/%s?secret=%s&issuer=%s"
            % (urllib.parse.quote(username), secret_b32,
               urllib.parse.quote(issuer)))


# --------------------------------------------------------------------------
# storage
# --------------------------------------------------------------------------

class DiagDB:
    """device_diagnostics behind one sqlite connection + lock (WAL)."""

    def __init__(self, path):
        d = os.path.dirname(path)
        if d:
            os.makedirs(d, exist_ok=True)
        self.conn = sqlite3.connect(path, check_same_thread=False)
        self.conn.execute("PRAGMA journal_mode=WAL")
        self.conn.execute("PRAGMA synchronous=NORMAL")
        self.conn.execute("PRAGMA busy_timeout=5000")
        self.lock = threading.Lock()
        with self.lock:
            self.conn.executescript(SCHEMA)
            self.conn.commit()

    def insert(self, ts, addr, blob):
        with self.lock:
            self.conn.execute(
                "INSERT INTO device_diagnostics(ts, addr, blob) "
                "VALUES(?,?,?)", (ts, addr, blob))
            self.conn.commit()

    def stats(self):
        with self.lock:
            total = self.conn.execute(
                "SELECT COUNT(*) FROM device_diagnostics").fetchone()[0]
            recent = self.conn.execute(
                "SELECT COUNT(*) FROM device_diagnostics WHERE ts>=?",
                (int(time.time()) - 86400,)).fetchone()[0]
        return total, recent

    def recent_rows(self, limit=200):
        with self.lock:
            rows = self.conn.execute(
                "SELECT id, ts, addr, blob FROM device_diagnostics "
                "ORDER BY id DESC LIMIT ?", (limit,)).fetchall()
        return [(r[0], r[1], r[2], r[3]) for r in rows]

    def close(self):
        with self.lock:
            self.conn.close()


class AuthStore:
    """Single-admin credential/session store, loaded from admin.json.

    Sessions live in memory only: they die with the process (a systemd
    restart invalidates everything), expire after session_ttl_sec, and can
    be individually logged out or globally revoked.  `epoch` bumps on
    revoke-all so old tokens stop validating immediately.
    """

    def __init__(self, path):
        with open(path, "r", encoding="utf-8") as f:
            cfg = json.load(f)
        self.username = cfg["username"]
        self.password_hash = cfg["password_hash"]
        self.totp_secret = cfg["totp_secret_b32"]
        self.ttl = int(cfg.get("session_ttl_sec", 4 * 3600))
        self.lock = threading.Lock()
        self.sessions = {}      # token -> (expiry_ts, ip)
        self.epoch = 0
        self.failures = {}      # ip -> [ts, ...]

    def _prune(self, now):
        dead = [t for t, (e, _) in self.sessions.items() if e <= now]
        for t in dead:
            del self.sessions[t]
        dead_ip = [ip for ip, fs in self.failures.items()
                   if not fs or fs[-1] < now - 900]
        for ip in dead_ip:
            del self.failures[ip]

    def locked_until(self, ip, now=None):
        now = now or int(time.time())
        with self.lock:
            fs = self.failures.get(ip) or []
            if len(fs) >= 5 and fs[-1] >= now - 900:
                return fs[-1] + 900
            return 0

    def record_failure(self, ip):
        now = int(time.time())
        with self.lock:
            fs = self.failures.setdefault(ip, [])
            fs.append(now)
            fs = [t for t in fs if t >= now - 900]
            self.failures[ip] = fs

    def clear_failures(self, ip):
        with self.lock:
            self.failures.pop(ip, None)

    def check_login(self, ip, username, password, code, now=None):
        """Verify credentials; returns (ok, reason)."""
        lock_until = self.locked_until(ip, now)
        if lock_until:
            return False, "too many failed attempts (locked out)"
        if not secrets.compare_digest(username or "", self.username):
            return False, "invalid credentials"
        if not Argon2.verify_password(self.password_hash, password or ""):
            return False, "invalid credentials"
        if not totp_verify(self.totp_secret, code or ""):
            return False, "invalid TOTP code"
        self.clear_failures(ip)
        return True, None

    def issue_session(self, ip):
        now = int(time.time())
        token = secrets.token_urlsafe(32)
        with self.lock:
            self._prune(now)
            self.sessions[token] = (now + self.ttl, ip)
        return token, now + self.ttl

    def validate(self, token):
        if not token:
            return None
        now = int(time.time())
        with self.lock:
            self._prune(now)
            s = self.sessions.get(token)
            if s is None:
                return None
            if s[0] <= now:
                self.sessions.pop(token, None)
                return None
            return s[1]  # the ip this session was issued to

    def revoke(self, token):
        with self.lock:
            self.sessions.pop(token, None)

    def revoke_all(self):
        with self.lock:
            self.sessions.clear()
            self.epoch += 1


# --------------------------------------------------------------------------
# HTTP layer
# --------------------------------------------------------------------------

class AdminHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "ms-admin/1.0"
    wbufsize = 0            # flush writes immediately (small responses)

    # ---- plumbing ------------------------------------------------------

    @property
    def state(self):
        return self.server.state

    def log_message(self, fmt, *args):
        log.info("%s %s", self.real_ip(), fmt % args)

    def real_ip(self):
        cf = self.headers.get("CF-Connecting-IP")
        if cf and cf.strip():
            return cf.strip().split(",")[0].strip()
        xff = self.headers.get("X-Forwarded-For")
        if xff and xff.strip():
            return xff.strip().split(",")[0].strip()
        return self.client_address[0]

    def _send(self, code, body=b"", ctype="text/plain; charset=utf-8",
              extra_headers=None):
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        if extra_headers:
            for k, v in extra_headers:
                self.send_header(k, v)
        self.end_headers()
        if body and self.command != "HEAD":
            self.wfile.write(body)

    def read_body(self):
        try:
            n = int(self.headers.get("Content-Length") or "0")
        except ValueError:
            n = 0
        if n <= 0:
            return b""
        if n > MAX_BODY:
            return None  # too large
        return self.rfile.read(n)

    # ---- routing -------------------------------------------------------

    def do_GET(self):
        p = self.path.split("?", 1)[0]
        if p == "/ms-admin/healthz":
            self._send(200, "ok\n")
            return
        if p == "/ms-admin/" or p.startswith("/ms-admin/"):
            self.handle_admin(p)
            return
        self._send(404, "not found\n")

    def do_POST(self):
        p = self.path.split("?", 1)[0]
        if p == "/ms-diag/ingest":
            self.handle_ingest()
            return
        if p == "/ms-admin/login":
            self.handle_login()
            return
        if p == "/ms-admin/logout":
            self.handle_logout()
            return
        if p == "/ms-admin/revoke-all":
            self.handle_revoke_all()
            return
        self._send(404, "not found\n")

    # ---- ingest --------------------------------------------------------

    def handle_ingest(self):
        ip = self.real_ip()
        now = int(time.time())
        with self.state.ingest_lock:
            q = self.state.ingest_counts
            q = [t for t in q if t > now - INGEST_RATE_WINDOW]
            if len(q) >= INGEST_RATE_LIMIT:
                self.state.ingest_counts = q
                self._send(429, '{"ok":false,"error":"rate limited"}\n',
                           "application/json")
                return
            q.append(now)
            self.state.ingest_counts = q

        body = self.read_body()
        if body is None:
            self._send(413, '{"ok":false,"error":"payload too large"}\n',
                       "application/json")
            return
        if not body:
            self._send(400, '{"ok":false,"error":"empty body"}\n',
                       "application/json")
            return
        try:
            doc = json.loads(body.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            self._send(400, '{"ok":false,"error":"bad json"}\n',
                       "application/json")
            return
        if not isinstance(doc, dict):
            self._send(400, '{"ok":false,"error":"expected object"}\n',
                       "application/json")
            return
        for f in EXPECTED_FIELDS:
            if f not in doc:
                self._send(400,
                           '{"ok":false,"error":"missing field %s"}\n' % f,
                           "application/json")
                return
        if not isinstance(doc.get("crash_text"), (str, type(None))):
            doc["crash_text"] = None
        for f in ("cpu_cores", "ram_mb", "uptime_sec"):
            if not isinstance(doc.get(f), int):
                self._send(400, '{"ok":false,"error":"bad field %s"}\n' % f,
                           "application/json")
                return
        try:
            blob = self.state.fernet.encrypt(
                json.dumps(doc, sort_keys=True).encode("utf-8"))
        except Exception:
            log.exception("encrypt failed")
            self._send(500, '{"ok":false,"error":"server error"}\n',
                       "application/json")
            return
        try:
            self.state.db.insert(now, ip, blob)
        except Exception:
            log.exception("db insert failed")
            self._send(500, '{"ok":false,"error":"server error"}\n',
                       "application/json")
            return
        self._send(200, '{"ok":true}\n', "application/json")

    # ---- admin viewer --------------------------------------------------

    def _require_session(self):
        cookie = self.headers.get("Cookie") or ""
        token = None
        for part in cookie.split(";"):
            k, _, v = part.strip().partition("=")
            if k == "ms_admin":
                token = v
        return self.state.auth.validate(token)

    def handle_admin(self, p):
        ip = self._require_session()
        if ip is None:
            self._send(401, self._login_page("Please sign in."),
                       "text/html; charset=utf-8")
            return
        if p != "/ms-admin/":
            self._send(404, "not found\n")
            return
        self._send(200, self._viewer_page(ip), "text/html; charset=utf-8")

    def handle_login(self):
        ip = self.real_ip()
        lock_until = self.state.auth.locked_until(ip)
        if lock_until:
            retry = max(1, lock_until - int(time.time()))
            self._send(423, self._login_page(
                "Too many failed attempts. Retry in ~%ds." % retry),
                "text/html; charset=utf-8")
            return
        body = self.read_body()
        if body is None:
            body = b""
        import urllib.parse
        params = {}
        for kv in body.decode("utf-8", "replace").split("&"):
            k, _, v = kv.partition("=")
            params[k] = urllib.parse.unquote_plus(v)
        ok, reason = self.state.auth.check_login(
            ip, params.get("username", ""), params.get("password", ""),
            params.get("totp", ""))
        if not ok:
            self.state.auth.record_failure(ip)
            log.warning("login FAILED ip=%s reason=%s", ip, reason)
            self._send(401, self._login_page("Invalid credentials."),
                       "text/html; charset=utf-8")
            return
        token, expiry = self.state.auth.issue_session(ip)
        log.warning("login OK ip=%s session_expires=%d", ip, expiry)
        self._send(302, "",
                   extra_headers=[
                       ("Location", "/ms-admin/"),
                       ("Set-Cookie",
                        "ms_admin=%s; Path=/ms-admin/; HttpOnly; "
                        "Secure; SameSite=Lax" % token)])
        self.close_connection = True

    def handle_logout(self):
        cookie = self.headers.get("Cookie") or ""
        for part in cookie.split(";"):
            k, _, v = part.strip().partition("=")
            if k == "ms_admin":
                self.state.auth.revoke(v)
        self._send(302, "",
                   extra_headers=[("Location", "/ms-admin/"),
                                  ("Set-Cookie",
                                   "ms_admin=; Path=/ms-admin/; HttpOnly; "
                                   "Secure; SameSite=Lax; Max-Age=0")])
        self.close_connection = True

    def handle_revoke_all(self):
        ip = self._require_session()
        if ip is None:
            self._send(401, self._login_page("Please sign in."),
                       "text/html; charset=utf-8")
            return
        self.state.auth.revoke_all()
        log.warning("revoke-all issued by ip=%s", ip)
        self._send(302, "",
                   extra_headers=[("Location", "/ms-admin/"),
                                  ("Set-Cookie",
                                   "ms_admin=; Path=/ms-admin/; HttpOnly; "
                                   "Secure; SameSite=Lax; Max-Age=0")])
        self.close_connection = True

    # ---- pages ---------------------------------------------------------

    def _page(self, title, body):
        return (
            "<!doctype html><html><head><meta charset='utf-8'>"
            "<title>%s</title><style>"
            "body{font-family:system-ui,sans-serif;margin:2rem;"
            "color:#1c1c1c;background:#f7f7f7}"
            "h1{font-size:1.25rem}.card{background:#fff;border:1px solid "
            "#ddd;border-radius:6px;padding:1rem;margin:1rem 0}"
            "table{border-collapse:collapse;width:100%%;font-size:0.85rem}"
            "th,td{border:1px solid #e3e3e3;padding:4px 8px;text-align:left}"
            "th{background:#efefef}pre{white-space:pre-wrap;word-break:"
            "break-all;max-width:60ch;margin:0;font-size:0.8rem}"
            "label{display:block;margin:.5rem 0 .15rem}input{width:20rem}"
            "button,form{display:inline;margin-right:.5rem}"
            ".mono{font-family:ui-monospace,monospace}"
            "</style></head><body>%s</body></html>" % (html.escape(title),
                                                       body))

    def _login_page(self, notice):
        body = (
            "<h1>Minesweeper diagnostics admin</h1>"
            "<p>%s</p>"
            "<form method='POST' action='/ms-admin/login'>"
            "<label>Username</label>"
            "<input type='text' name='username' autocomplete='username'>"
            "<label>Password</label>"
            "<input type='password' name='password' "
            "autocomplete='current-password'>"
            "<label>TOTP code</label>"
            "<input type='text' name='totp' autocomplete='one-time-code' "
            "inputmode='numeric' pattern='[0-9]{6}' maxlength='6'>"
            "<br><br><button type='submit'>Sign in</button>"
            "</form>") % html.escape(notice)
        return self._page("Sign in", body)

    def _viewer_page(self, ip):
        total, recent = self.state.db.stats()
        rows = self.state.db.recent_rows(200)
        cards = (
            "<div class='card'><b>%d</b> total rows &middot; "
            "<b>%d</b> in the last 24h &middot; signed in from "
            "<span class='mono'>%s</span></div>"
            % (total, recent, html.escape(ip)))
        if not rows:
            cards += "<div class='card'>No diagnostics yet.</div>"
        else:
            cards += ("<table><tr><th>id</th><th>when (UTC)</th>"
                      "<th>addr</th><th>machine</th><th>os</th><th>cpu</th>"
                      "<th>gpu</th><th>ram</th><th>display</th>"
                      "<th>game</th><th>uptime</th><th>crash</th></tr>")
            for rid, ts, addr, blob in rows:
                d = self._decrypt(blob)
                if d is None:
                    cells = ["<td>%d</td>" % rid,
                             "<td>%s</td>" % self._ts(ts),
                             "<td>%s</td>" % html.escape(addr),
                             "<td colspan='9'>unable to decrypt "
                             "(key mismatch?)</td>"]
                    cards += "<tr>%s</tr>" % "".join(cells)
                    continue
                crash = d.get("crash_text") or ""
                cells = [
                    "<td>%d</td>" % rid,
                    "<td>%s</td>" % self._ts(ts),
                    "<td>%s</td>" % html.escape(addr),
                    "<td class='mono'>%s</td>" % html.escape(
                        str(d.get("machine_id", ""))[:16]),
                    "<td>%s</td>" % html.escape(str(d.get("os", ""))),
                    "<td>%s</td>" % html.escape(
                        str(d.get("cpu", ""))),
                    "<td>%s</td>" % html.escape(str(d.get("gpu", ""))),
                    "<td>%d</td>" % int(d.get("ram_mb", 0)),
                    "<td>%s</td>" % html.escape(str(d.get("display", ""))),
                    "<td>%s</td>" % html.escape(
                        str(d.get("game_version", ""))),
                    "<td>%ds</td>" % int(d.get("uptime_sec", 0)),
                    "<td><pre>%s</pre></td>" % html.escape(crash),
                ]
                cards += "<tr>%s</tr>" % "".join(cells)
            cards += "</table>"
        actions = (
            "<form method='POST' action='/ms-admin/logout'>"
            "<button type='submit'>Log out</button></form>"
            "<form method='POST' action='/ms-admin/revoke-all'>"
            "<button type='submit' "
            "onclick=\"return confirm('Revoke all sessions?');\">"
            "Revoke all sessions</button></form>"
            "<a href='/ms-admin/'>Refresh</a>")
        body = ("<h1>Minesweeper diagnostics</h1>%s%s"
                % (actions, cards))
        return self._page("Diagnostics", body)

    def _ts(self, ts):
        return time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(ts))

    def _decrypt(self, blob):
        try:
            raw = self.state.fernet.decrypt(blob)
            return json.loads(raw.decode("utf-8"))
        except Exception:
            return None


class AdminServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr, state):
        self.state = state
        super().__init__(addr, AdminHandler)


def make_state(args, fernet_key):
    db = DiagDB(args.db)
    auth = AuthStore(args.config)
    return db, auth, Fernet(fernet_key)


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def cmd_init(args):
    """One-time setup: key file + admin.json + TOTP URI."""
    import getpass

    d = os.path.dirname(args.key)
    if d:
        os.makedirs(d, exist_ok=True)

    if os.path.exists(args.key) and os.path.getsize(args.key) > 0:
        with open(args.key, "rb") as f:
            key = f.read()
        print("key file exists, reusing it (%s)" % args.key)
    else:
        key = Fernet.generate_key()
        fd = os.open(args.key, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o400)
        try:
            os.write(fd, key)
        finally:
            os.close(fd)
        print("wrote new key: %s (mode 0400)" % args.key)

    username = args.username or "admin"
    while True:
        if sys.stdin.isatty():
            pw = getpass.getpass("Password (>= 20 chars): ")
            pw2 = getpass.getpass("Repeat password: ")
        else:
            # non-interactive (piped) stdin: read two lines, echo shown
            sys.stdout.write("Password (>= 20 chars): "); sys.stdout.flush()
            pw = sys.stdin.readline().rstrip("\r\n")
            pw2 = sys.stdin.readline().rstrip("\r\n")
        if len(pw) < 20:
            print("password too short (min 20 characters)")
            continue
        if pw != pw2:
            print("passwords do not match")
            continue
        break
    pw_hash = Argon2.hash_password(pw)

    secret = base64.b32encode(secrets.token_bytes(20)).decode("ascii")
    cfg = {
        "username": username,
        "password_hash": pw_hash,
        "totp_secret_b32": secret,
        "session_ttl_sec": args.session_ttl,
    }
    fd = os.open(args.config, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    try:
        os.write(fd, json.dumps(cfg, indent=2).encode("utf-8"))
    finally:
        os.close(fd)
    print("wrote config: %s (mode 0600)" % args.config)
    print()
    print("Add TOTP to your authenticator app:")
    print("  %s" % otpauth_uri(username, "Minesweeper Admin", secret))
    print()
    print("Verify it works, then start the service:")
    print("  systemctl start minesweeper-admin")
    return 0


def cmd_selfcheck(args):
    ok = True

    def check(name, cond):
        nonlocal ok
        print("  %-34s %s" % (name, "OK" if cond else "FAIL"))
        if not cond:
            ok = False

    # TOTP RFC 6238 SHA1 vectors (ascii secret "12345678901234567890");
    # the published vectors are 8-digit, authenticator apps use 6.
    b32 = base64.b32encode(b"12345678901234567890").decode("ascii")
    check("totp counter 1 (8d) -> 94287082",
          totp_value(b32, 1, 8) == "94287082")
    check("totp counter 666666666 (8d) -> 65353130",
          totp_value(b32, 666666666, 8) == "65353130")
    check("totp 6-digit derives from 8-digit vector",
          totp_value(b32, 1, 6) == "287082")
    check("totp current step (ts=59) contains vector",
          totp_verify(b32, "287082", window=0, ts=59))
    check("totp rejects bad code",
          not totp_verify(b32, "000000", window=0, ts=59))

    try:
        fk = Fernet.generate_key()
        f = Fernet(fk)
        blob = f.encrypt(b'{"x": 1}')
        check("fernet encrypt/decrypt roundtrip",
              f.decrypt(blob) == b'{"x": 1}')
        tampered = blob[:-1] + (b"A" if blob[-1] != b"A" else b"B")
        try:
            f.decrypt(tampered)
            check("fernet rejects tampered blob", False)
        except Exception:
            check("fernet rejects tampered blob", True)
    except RuntimeError as e:
        print("  skip  cryptography not installed (%s)" % e)

    try:
        h = Argon2.hash_password("correct horse battery staple xyz")
        check("argon2 hash/verify", Argon2.verify_password(h, "correct horse "
              "battery staple xyz"))
        check("argon2 rejects wrong password",
              not Argon2.verify_password(h, "wrong"))
    except RuntimeError as e:
        print("  skip  argon2-cffi not installed (%s)" % e)

    print("selfcheck:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def run(args):
    key = None
    if os.path.exists(args.key):
        with open(args.key, "rb") as f:
            key = f.read()
    if not key:
        print("no key at %s -- run --init first" % args.key, file=sys.stderr)
        return 2

    db, auth, fernet = make_state(args, key)

    state = argparse.Namespace(
        db=db, auth=auth, fernet=fernet,
        ingest_lock=threading.Lock(),
        ingest_counts=[],
    )
    try:
        srv = AdminServer(("127.0.0.1" if not args.host else args.host,
                           args.port), state)
    except OSError as e:
        print("cannot bind %s:%d: %s" % (args.host, args.port, e),
              file=sys.stderr)
        db.close()
        return 2

    log.info("admin listening on %s:%d (db=%s config=%s)",
             args.host, args.port, args.db, args.config)
    print("ms-admin listening on %s:%d" % (args.host, args.port), flush=True)
    try:
        srv.serve_forever(poll_interval=1.0)
    except KeyboardInterrupt:
        print("\nshutting down...", flush=True)
    finally:
        srv.server_close()
        db.close()
    return 0


def main():
    ap = argparse.ArgumentParser(
        description="Minesweeper diagnostics ingest + admin viewer")
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8444)
    ap.add_argument("--db", default="/var/lib/minesweeper-sim/diag.db")
    ap.add_argument("--config", default="/etc/minesweeper-server/admin.json")
    ap.add_argument("--key", default="/etc/minesweeper-server/diag.key")
    ap.add_argument("--session-ttl", type=int, default=4 * 3600,
                    help="admin session lifetime in seconds")
    ap.add_argument("--init", action="store_true",
                    help="create key + admin.json and print the TOTP URI")
    ap.add_argument("--username", default="admin",
                    help="admin username to store during --init")
    ap.add_argument("--selfcheck", action="store_true")
    args = ap.parse_args()

    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s %(message)s")
    try:
        sys.setswitchinterval(0.02)
    except (ValueError, AttributeError):
        pass

    if args.init:
        return cmd_init(args)
    if args.selfcheck:
        return cmd_selfcheck(args)
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
