// server.js - HTTP layer for the diagnostics ingest + admin viewer.
// Zero-dependency port of admin.py's AdminHandler/AdminServer.  TLS is
// terminated by nginx, so this listens on loopback only.  device_diagnostics
// payloads are opaque encrypted blobs; the admin viewer decrypts on read.

import http from "node:http";

export const MAX_BODY = 65536;
const INGEST_RATE_LIMIT = 20;
const INGEST_RATE_WINDOW = 10;

const EXPECTED_FIELDS = [
  "machine_id",
  "os",
  "cpu",
  "cpu_cores",
  "gpu",
  "ram_mb",
  "display",
  "game_version",
  "uptime_sec",
  "crash_text",
];

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => {
    const m = {
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#x27;",
    };
    return m[c];
  });
}

function realIp(req) {
  const cf = req.headers["cf-connecting-ip"];
  if (cf && cf.trim()) return cf.trim().split(",")[0].trim();
  const xff = req.headers["x-forwarded-for"];
  if (xff && xff.trim()) return xff.trim().split(",")[0].trim();
  return req.socket.remoteAddress ?? "";
}

function cookieToken(req) {
  const cookie = req.headers.cookie ?? "";
  for (const part of cookie.split(";")) {
    const i = part.indexOf("=");
    if (i < 0) continue;
    if (part.slice(0, i).trim() === "ms_admin") {
      return part.slice(i + 1).trim();
    }
  }
  return null;
}

function send(res, code, body = "", ctype = "text/plain; charset=utf-8", extraHeaders = null, method = null) {
  const buf = Buffer.isBuffer(body) ? body : Buffer.from(body, "utf8");
  res.statusCode = code;
  res.setHeader("Content-Type", ctype);
  res.setHeader("Content-Length", String(buf.length));
  res.setHeader("Connection", "close");
  if (extraHeaders) {
    for (const [k, v] of extraHeaders) res.setHeader(k, v);
  }
  if (!(method === "HEAD") && buf.length) res.end(buf);
  else res.end();
}

function sendJson(res, code, obj) {
  send(res, code, JSON.stringify(obj) + "\n", "application/json");
}

async function readBody(req) {
  const raw = req.headers["content-length"];
  let n = 0;
  try {
    n = Number(raw ?? 0);
  } catch {
    n = 0;
  }
  if (!Number.isFinite(n) || n <= 0) return Buffer.alloc(0);
  if (n > MAX_BODY) return null;
  const chunks = [];
  let got = 0;
  for await (const chunk of req) {
    got += chunk.length;
    if (got > MAX_BODY) return null;
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

function utcTs(ts) {
  const d = new Date(ts * 1000);
  const p = (n) => String(n).padStart(2, "0");
  return (
    d.getUTCFullYear() +
    "-" + p(d.getUTCMonth() + 1) +
    "-" + p(d.getUTCDate()) +
    " " + p(d.getUTCHours()) +
    ":" + p(d.getUTCMinutes()) +
    ":" + p(d.getUTCSeconds())
  );
}

// --------------------------------------------------------------------------
// ingest
// --------------------------------------------------------------------------

async function handleIngest(req, res, state) {
  const ip = realIp(req);
  const now = Math.floor(Date.now() / 1000);
  state.ingestCounts = state.ingestCounts.filter(
    (t) => t > now - INGEST_RATE_WINDOW
  );
  if (state.ingestCounts.length >= INGEST_RATE_LIMIT) {
    sendJson(res, 429, { ok: false, error: "rate limited" });
    return;
  }
  state.ingestCounts.push(now);

  const body = await readBody(req);
  if (body === null) {
    sendJson(res, 413, { ok: false, error: "payload too large" });
    return;
  }
  if (!body.length) {
    sendJson(res, 400, { ok: false, error: "empty body" });
    return;
  }
  let doc;
  try {
    doc = JSON.parse(body.toString("utf8"));
  } catch {
    sendJson(res, 400, { ok: false, error: "bad json" });
    return;
  }
  if (typeof doc !== "object" || doc === null || Array.isArray(doc)) {
    sendJson(res, 400, { ok: false, error: "expected object" });
    return;
  }
  for (const f of EXPECTED_FIELDS) {
    if (!(f in doc)) {
      sendJson(res, 400, { ok: false, error: "missing field " + f });
      return;
    }
  }
  if (typeof doc.crash_text !== "string" && doc.crash_text !== null) {
    doc.crash_text = null;
  }
  for (const f of ["cpu_cores", "ram_mb", "uptime_sec"]) {
    if (!Number.isInteger(doc[f])) {
      sendJson(res, 400, { ok: false, error: "bad field " + f });
      return;
    }
  }
  let blob;
  try {
    const sorted = {};
    for (const k of Object.keys(doc).sort()) sorted[k] = doc[k];
    blob = state.fernet.encrypt(Buffer.from(JSON.stringify(sorted), "utf8"));
  } catch (e) {
    console.error("encrypt failed: " + e.message);
    sendJson(res, 500, { ok: false, error: "server error" });
    return;
  }
  try {
    state.db.insert(now, ip, blob);
  } catch (e) {
    console.error("db insert failed: " + e.message);
    sendJson(res, 500, { ok: false, error: "server error" });
    return;
  }
  sendJson(res, 200, { ok: true });
}

// --------------------------------------------------------------------------
// admin viewer
// --------------------------------------------------------------------------

function requireSession(req, state) {
  return state.auth.validate(cookieToken(req));
}

function page(title, body) {
  return (
    "<!doctype html><html><head><meta charset='utf-8'>" +
    "<title>" + escapeHtml(title) + "</title><style>" +
    "body{font-family:system-ui,sans-serif;margin:2rem;color:#1c1c1c;background:#f7f7f7}" +
    "h1{font-size:1.25rem}.card{background:#fff;border:1px solid #ddd;" +
    "border-radius:6px;padding:1rem;margin:1rem 0}" +
    "table{border-collapse:collapse;width:100%;font-size:0.85rem}" +
    "th,td{border:1px solid #e3e3e3;padding:4px 8px;text-align:left}" +
    "th{background:#efefef}pre{white-space:pre-wrap;word-break:break-all;" +
    "max-width:60ch;margin:0;font-size:0.8rem}" +
    "label{display:block;margin:.5rem 0 .15rem}input{width:20rem}" +
    "button,form{display:inline;margin-right:.5rem}" +
    ".mono{font-family:ui-monospace,monospace}" +
    "</style></head><body>" + body + "</body></html>"
  );
}

function loginPage(notice) {
  const body =
    "<h1>Minesweeper diagnostics admin</h1>" +
    "<p>" + escapeHtml(notice) + "</p>" +
    "<form method='POST' action='/ms-admin/login'>" +
    "<label>Username</label>" +
    "<input type='text' name='username' autocomplete='username'>" +
    "<label>Password</label>" +
    "<input type='password' name='password' autocomplete='current-password'>" +
    "<label>TOTP code</label>" +
    "<input type='text' name='totp' autocomplete='one-time-code' " +
    "inputmode='numeric' pattern='[0-9]{6}' maxlength='6'>" +
    "<br><br><button type='submit'>Sign in</button>" +
    "</form>";
  return page("Sign in", body);
}

function viewerPage(state, ip) {
  const [total, recent] = state.db.stats();
  const rows = state.db.recentRows(200);
  let cards =
    "<div class='card'><b>" + total + "</b> total rows &middot; <b>" + recent +
    "</b> in the last 24h &middot; signed in from " +
    "<span class='mono'>" + escapeHtml(ip) + "</span></div>";
  if (!rows.length) {
    cards += "<div class='card'>No diagnostics yet.</div>";
  } else {
    cards +=
      "<table><tr><th>id</th><th>when (UTC)</th><th>addr</th><th>machine</th>" +
      "<th>os</th><th>cpu</th><th>gpu</th><th>ram</th><th>display</th>" +
      "<th>game</th><th>uptime</th><th>crash</th></tr>";
    for (const [rid, ts, addr, blob] of rows) {
      const d = decryptRow(state, blob);
      if (d === null) {
        cards +=
          "<tr><td>" + rid + "</td><td>" + utcTs(ts) + "</td><td>" +
          escapeHtml(addr) + "</td><td colspan='9'>unable to decrypt " +
          "(key mismatch?)</td></tr>";
        continue;
      }
      const crash = d.crash_text || "";
      cards +=
        "<tr>" +
        "<td>" + rid + "</td><td>" + utcTs(ts) + "</td><td>" + escapeHtml(addr) +
        "</td>" +
        "<td class='mono'>" + escapeHtml(String(d.machine_id ?? "")).slice(0, 16) +
        "</td>" +
        "<td>" + escapeHtml(String(d.os ?? "")) + "</td>" +
        "<td>" + escapeHtml(String(d.cpu ?? "")) + "</td>" +
        "<td>" + escapeHtml(String(d.gpu ?? "")) + "</td>" +
        "<td>" + Number(d.ram_mb ?? 0) + "</td>" +
        "<td>" + escapeHtml(String(d.display ?? "")) + "</td>" +
        "<td>" + escapeHtml(String(d.game_version ?? "")) + "</td>" +
        "<td>" + Number(d.uptime_sec ?? 0) + "s</td>" +
        "<td><pre>" + escapeHtml(crash) + "</pre></td>" +
        "</tr>";
    }
    cards += "</table>";
  }
  const actions =
    "<form method='POST' action='/ms-admin/logout'>" +
    "<button type='submit'>Log out</button></form>" +
    "<form method='POST' action='/ms-admin/revoke-all'>" +
    "<button type='submit' onclick=\"return confirm('Revoke all sessions?');\">" +
    "Revoke all sessions</button></form>" +
    "<a href='/ms-admin/'>Refresh</a>";
  return page(
    "Diagnostics",
    "<h1>Minesweeper diagnostics</h1>" + actions + cards
  );
}

function decryptRow(state, blob) {
  try {
    const raw = state.fernet.decrypt(blob);
    return JSON.parse(raw.toString("utf8"));
  } catch {
    return null;
  }
}

const CLEAR_COOKIE = "ms_admin=; Path=/ms-admin/; HttpOnly; Secure; SameSite=Lax; Max-Age=0";

function handleAdmin(req, res, p, state) {
  const ip = requireSession(req, state);
  if (ip === null) {
    send(res, 401, loginPage("Please sign in."), "text/html; charset=utf-8");
    return;
  }
  if (p !== "/ms-admin/") {
    send(res, 404, "not found\n");
    return;
  }
  send(res, 200, viewerPage(state, ip), "text/html; charset=utf-8");
}

async function handleLogin(req, res, state) {
  const ip = realIp(req);
  const lockUntil = state.auth.lockedUntil(ip);
  if (lockUntil) {
    const retry = Math.max(1, lockUntil - Math.floor(Date.now() / 1000));
    send(
      res,
      423,
      loginPage("Too many failed attempts. Retry in ~" + retry + "s."),
      "text/html; charset=utf-8"
    );
    return;
  }
  const body = await readBody(req);
  const params = new URLSearchParams((body ?? Buffer.alloc(0)).toString("utf8"));
  const [ok, reason] = state.auth.checkLogin(
    ip,
    params.get("username") ?? "",
    params.get("password") ?? "",
    params.get("totp") ?? ""
  );
  if (!ok) {
    state.auth.recordFailure(ip);
    console.warn("login FAILED ip=" + ip + " reason=" + reason);
    send(res, 401, loginPage("Invalid credentials."), "text/html; charset=utf-8");
    return;
  }
  const [token, expiry] = state.auth.issueSession(ip);
  console.warn("login OK ip=" + ip + " session_expires=" + expiry);
  send(res, 302, "", "text/plain; charset=utf-8", [
    ["Location", "/ms-admin/"],
    ["Set-Cookie", "ms_admin=" + token + "; Path=/ms-admin/; HttpOnly; Secure; SameSite=Lax"],
  ]);
}

function handleLogout(req, res, state) {
  const token = cookieToken(req);
  if (token) state.auth.revoke(token);
  send(res, 302, "", "text/plain; charset=utf-8", [
    ["Location", "/ms-admin/"],
    ["Set-Cookie", CLEAR_COOKIE],
  ]);
}

function handleRevokeAll(req, res, state) {
  const ip = requireSession(req, state);
  if (ip === null) {
    send(res, 401, loginPage("Please sign in."), "text/html; charset=utf-8");
    return;
  }
  state.auth.revokeAll();
  console.warn("revoke-all issued by ip=" + ip);
  send(res, 302, "", "text/plain; charset=utf-8", [
    ["Location", "/ms-admin/"],
    ["Set-Cookie", CLEAR_COOKIE],
  ]);
}

async function handleRequest(req, res, state) {
  const ip = realIp(req);
  console.log(ip + " " + req.method + " " + req.url);
  const p = req.url.split("?", 1)[0];
  if (req.method === "GET") {
    if (p === "/ms-admin/healthz") return send(res, 200, "ok\n");
    if (p === "/ms-admin/" || p.startsWith("/ms-admin/")) {
      return handleAdmin(req, res, p, state);
    }
    return send(res, 404, "not found\n");
  }
  if (req.method === "POST") {
    if (p === "/ms-diag/ingest") return handleIngest(req, res, state);
    if (p === "/ms-admin/login") return handleLogin(req, res, state);
    if (p === "/ms-admin/logout") return handleLogout(req, res, state);
    if (p === "/ms-admin/revoke-all") return handleRevokeAll(req, res, state);
    return send(res, 404, "not found\n");
  }
  send(res, 501, "Unsupported method\n");
}

export function createAdminServer(state) {
  const server = http.createServer((req, res) => {
    void handleRequest(req, res, state).catch((e) => {
      console.error("handler error: " + (e && e.stack ? e.stack : e));
      try {
        send(res, 500, "internal error\n");
      } catch {
        // response already sent
      }
    });
  });
  return server;
}
