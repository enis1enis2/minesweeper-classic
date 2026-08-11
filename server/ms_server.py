"""ms_server.py - distributed simulation / telemetry server for Minesweeper.

The server runs headless simulated games (sim_engine.SimBoard driven by
ms_solver.play_game) and, for every connected client:

  * streams a `seed <diff> <n>` line before each simulated game starts, so a
    running Minesweeper telemetry client re-rolls its board to that seed;
  * streams an `outcome <diff> <seed> <won> <moves> <time_ms> <guesses>`
    line when the simulated game finishes.

Clients send `metric ...` lines; each is timestamped and stored in SQLite.

Seed-request system (accurate, controllable win/loss analysis).  In addition
to the broadcast stream, a client can ask the server to play specific games
and stream the results only to itself:

  Client -> server:
      reqseed  <diff> <n> [count]   play seed n (optionally count times)
      reqbatch <diff> <count>       play `count` random seeds at this difficulty
      requntil <diff> <n> [max]     replay seed n until a loss is seen (cap max)

  Server -> that client, per requested game:
      reqwait   <diff> <n>            this request is queued (heavy request)
      reqgame   <diff> <n>
      seed      <diff> <n>
      outcome   <diff> <n> <won> <moves> <time_ms> <guesses>
  ...a final marker:
      lossfound <diff> <n> <run> <won> <moves> <time_ms> <guesses>   (requntil)
      noloss    <diff> <n> <max>                                      (requntil)
      reqdone  <diff> <count>

Requested games are recorded in sim_games with the requesting address in the
`requester` column (NULL for broadcast games), so DB queries can compute
per-requester win/loss rates.  Requests run alongside the broadcast stream on
per-client worker threads.

Threading model:
  * API accept thread: owns the listen socket, spawns a per-connection
    thread that (a) reads `metric` lines and stores them, (b) accepts
    `reqseed`/`reqbatch`/`requntil` lines and hands them to the requesting
    client's request worker, and (c) serves as the target for streamed
    seed/outcome lines.
  * Producer thread: repeatedly plays one simulated game, then broadcasts
    seed -> outcome to every live connection.  Pauses while no client is
    connected so seeds are never produced into the void.
  * Request workers: one thread per requesting client consumes that client's
    requests FIFO, playing each requested game and streaming the result only
    to that client.  Because each client gets its own worker, a long
    `requntil` on one connection never delays another client's requests.
  * Admission gate: the GIL caps total Python CPU at ~1 core no matter how
    many threads run, so N concurrent heavy requests would each slow down by
    ~N.  A fair FIFO gate (--max-concurrent) admits only a bounded number of
    heavy requests at once, in arrival order, so the first-arrived heavy
    request runs at ~full speed and later ones wait their turn (a `reqwait`
    line tells the client it is queued).  Heavy = a request whose estimated
    CPU is >= HEAVY_CPU_SECONDS; light requests (single sims, small batches)
    skip the gate entirely and stay instant.

Usage:
  python3 ms_server.py [--host 0.0.0.0] [--port 28571] [--db data/sim.db]
      [--rate 5] [--difficulty all|beginner|intermediate|expert]
      [--seed 12345] [--max-request 10000] [--max-concurrent 1]
      [--solver-user USER --solver-pass PASS | --solver-config FILE]

Solver protection:
  The seed-request system (reqseed/reqbatch/requntil) is the "solver": it
  runs the solver on demand for any connected client.  It is protected by
  default -- when no credentials are configured it is disabled and every
  solver request is answered with `reqdenied`.  Configure credentials via
  --solver-user/--solver-pass, or --solver-config (JSON {"user":..,"pass":..}),
  or the MS_SOLVER_USER / MS_SOLVER_PASS environment variables (args win
  over config, config wins over env).  A client then authenticates per
  connection with a challenge-response handshake (HMAC-SHA256 over a server
  nonce, so the password is never sent):

      client:  auth <user>
      server:  authchal <nonce-hex>
      client:  authresp <hmac-sha256-hex>
      server:  authok   (or autherr; 5 failures closes the connection)

  The broadcast seed/outcome stream and metric ingestion stay open to every
  client; only the on-demand solver requests are gated.

Leaderboard (best win times, classic Hall-of-Fame style):
      client:  lbscore <name> <diff> <time_ms>
      server:  lbstored <rank> <diff> <name> <time_ms>   (improved / new)
               lbnotop                                    (existing time faster)
      client:  lbtop <count>            (best across all difficulties)
               lbtop <diff> <count>     (best on one difficulty)
      server:  lbtop [<diff>] <count>   header
               lbentry <rank> <diff> <name> <time_ms> <ts>
               lbdone
  Names are [A-Za-z0-9_-]{1,16}; per (name, difficulty) only the fastest
  time is kept.  Submissions are rate-limited per IP (default 20/min).

Run `python3 ms_server.py --selfcheck` for a quick local sanity pass.
"""

import argparse
import hashlib
import hmac
import json
import os
import queue
import random
import re
import socket
import sqlite3
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
# The solver lives in the repo as ../minesweeper_bot (local layout) and in
# the deployed layout as ./minesweeper_bot inside the install dir.  Try both.
for _p in (os.path.join(os.path.dirname(HERE), "minesweeper_bot"),
           os.path.join(HERE, "minesweeper_bot")):
    if os.path.isdir(_p) and _p not in sys.path:
        sys.path.insert(0, _p)

from ms_solver import play_game          # noqa: E402
from sim_engine import PRESETS, SimBoard, SimClient  # noqa: E402

SOLVER_STRATEGY = {
    "tiebreak": "info",
    "first": "center",
    "use_chord": True,
    "refresh": False,
}
DIFFS = ["beginner", "intermediate", "expert"]

# Estimated CPU per simulated game, measured on the production box
# (4 vCPU Haswell, CPython 3.12).  Used to classify a request as "heavy"
# (worth gating) vs "light" (bypasses the gate and stays instant).
GAME_CPU_SECONDS = {"beginner": 0.002, "intermediate": 0.016, "expert": 0.076}
# A request estimated to take >= this much CPU is heavy.  At the defaults a
# single sim (beginner 0.002s / intermediate 0.016s / expert 0.076s) is
# always light; only multi-game batches and long requntil runs are gated.
#
# Known edge case (deliberately accepted, not gated): the estimate is a fixed
# average, so requests just below the threshold bypass the gate -- e.g.
# count=3 at expert is 3*0.076=0.228s < 0.25s, and count=13 at intermediate
# is 0.208s.  Under load a handful of such "light" requests can stack for
# ~200ms of uncontended CPU.  This is accepted because light requests are
# typically single sims / small batches from interactive clients, and heavy
# floods are already served FIFO through the gate.  Closing the gap would
# mean lowering HEAVY_CPU_SECONDS (gates more traffic and changes reqwait
# behavior for currently-bypassing requests) or adding a difficulty-specific
# minimum count -- intentionally not done.
HEAVY_CPU_SECONDS = 0.25

# solver-authentication challenge: nonce freshness and brute-force lockout
NONCE_TTL = 60
MAX_AUTH_FAILS = 5
# a single inbound line may never exceed this; a client that sends more than
# this without a newline is disconnected (guards the conn_thread read buffer
# against unbounded memory growth).  Real lines (metric/auth/lb*) are tiny.
MAX_LINE = 65536
# leaderboard submission rate limit (per source IP)
LB_WINDOW = 60.0
LB_MAX = 20
# cap on distinct source IPs tracked for rate limiting; beyond this the
# least-recently-active entries are evicted so a flood of distinct IPs
# cannot grow the tracking dict without bound
LB_MAX_IPS = 4096
# leaderboard player names: letters/digits/underscore/dash, 1..16 chars
NAME_RE = re.compile(r"^[A-Za-z0-9_-]{1,16}$")

SCHEMA = """
CREATE TABLE IF NOT EXISTS sim_games(
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    difficulty TEXT NOT NULL,
    seed INTEGER NOT NULL,
    won INTEGER NOT NULL,
    moves INTEGER NOT NULL,
    time_ms INTEGER NOT NULL,
    guesses INTEGER NOT NULL,
    chords INTEGER NOT NULL,
    flags INTEGER NOT NULL,
    deduce_batches INTEGER NOT NULL,
    frontier TEXT,
    wall_ms INTEGER NOT NULL,
    requester TEXT
);
CREATE TABLE IF NOT EXISTS leaderboard(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    difficulty TEXT NOT NULL,
    time_ms INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    UNIQUE(name, difficulty)
);
CREATE INDEX IF NOT EXISTS idx_leaderboard_diff ON
    leaderboard(difficulty, time_ms);
CREATE TABLE IF NOT EXISTS client_metrics(
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    addr TEXT NOT NULL,
    line TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_metrics_ts ON client_metrics(ts);
CREATE TABLE IF NOT EXISTS clients(
    addr TEXT PRIMARY KEY,
    connect_ts INTEGER NOT NULL,
    last_ts INTEGER NOT NULL,
    seeds_sent INTEGER NOT NULL,
    outcomes_sent INTEGER NOT NULL,
    active INTEGER NOT NULL DEFAULT 1
);
"""


class Database:
    """Single sqlite connection behind a lock (WAL for concurrent writers).

    WAL + synchronous=NORMAL: commits append to the WAL without an fsync per
    write (~16x faster than the default FULL; crash-safe, only the last few
    transactions can be lost on sudden power failure, which is fine for
    telemetry).  Games/metrics totals are kept in memory so the per-second
    status line does not scan the whole table every time.
    """

    def __init__(self, path):
        d = os.path.dirname(path)
        if d:
            os.makedirs(d, exist_ok=True)
        self.conn = sqlite3.connect(path, check_same_thread=False)
        self.conn.execute("PRAGMA journal_mode=WAL")
        self.conn.execute("PRAGMA synchronous=NORMAL")
        self.conn.execute("PRAGMA busy_timeout=5000")
        self.conn.execute("PRAGMA cache_size=-16000")
        self.lock = threading.Lock()
        self._games = 0
        self._wins = 0
        self._metrics = 0
        with self.lock:
            self.conn.executescript(SCHEMA)
            # migration for databases created before the requester column
            try:
                self.conn.execute(
                    "ALTER TABLE sim_games ADD COLUMN requester TEXT")
                self.conn.commit()
            except sqlite3.OperationalError:
                pass
            # warm the in-memory counters from existing rows on startup
            g = self.conn.execute(
                "SELECT COUNT(*), COALESCE(SUM(won),0) FROM sim_games").fetchone()
            m = self.conn.execute(
                "SELECT COUNT(*) FROM client_metrics").fetchone()
            self._games, self._wins = int(g[0]), int(g[1])
            self._metrics = int(m[0])

    def record_game(self, g):
        with self.lock:
            self.conn.execute(
                "INSERT INTO sim_games(ts,difficulty,seed,won,moves,time_ms,"
                "guesses,chords,flags,deduce_batches,frontier,wall_ms,"
                "requester) "
                "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (g["ts"], g["difficulty"], g["seed"], int(g["won"]),
                 g["moves"], g["time_ms"], g["guesses"], g["chords"],
                 g["flags"], g["deduce_batches"],
                 json.dumps(g["frontier"]), g["wall_ms"],
                 g.get("requester")))
            self.conn.commit()
            self._games += 1
            self._wins += int(g["won"])

    def record_metric(self, ts, addr, line):
        with self.lock:
            self.conn.execute(
                "INSERT INTO client_metrics(ts,addr,line) VALUES(?,?,?)",
                (ts, addr, line))
            self.conn.commit()
            self._metrics += 1

    def upsert_client(self, addr, connect_ts, active=True):
        with self.lock:
            self.conn.execute(
                "INSERT INTO clients(addr,connect_ts,last_ts,seeds_sent,"
                "outcomes_sent,active) VALUES(?,?,?,0,0,?) "
                "ON CONFLICT(addr) DO UPDATE SET active=?",
                (addr, connect_ts, connect_ts, int(active), int(active)))
            self.conn.commit()

    def client_touch(self, addr, seeds, outcomes):
        with self.lock:
            self.conn.execute(
                "UPDATE clients SET last_ts=?, seeds_sent=?, "
                "outcomes_sent=? WHERE addr=?",
                (int(time.time()), seeds, outcomes, addr))
            self.conn.commit()

    def client_touch_many(self, rows):
        """Batch version of client_touch: one transaction for many rows.

        broadcast() touches every live client each tick, so committing once
        instead of N times cuts the per-broadcast commit count from N to 1
        (the same write-amplification the WAL/NORMAL pragmas were aimed at)."""
        if not rows:
            return
        now = int(time.time())
        with self.lock:
            self.conn.executemany(
                "UPDATE clients SET last_ts=?, seeds_sent=?, "
                "outcomes_sent=? WHERE addr=?",
                [(now, s, o, a) for (a, s, o) in rows])
            self.conn.commit()

    def counts(self):
        """Totals kept in memory (no per-second full-table scans).  Active
        client count comes from the clients table."""
        with self.lock:
            c = self.conn.execute(
                "SELECT COUNT(*) FROM clients WHERE active=1").fetchone()
        return (self._games, self._wins), (self._metrics,), c

    def record_score(self, name, diff, time_ms):
        """Record a leaderboard time (best time per name+difficulty wins).

        Returns (improved, rank): improved is False when the player's
        existing entry is already faster or equal; rank is the 1-based
        position of the player's current best time on this difficulty."""
        with self.lock:
            cur = self.conn.execute(
                "SELECT id, time_ms FROM leaderboard "
                "WHERE name=? AND difficulty=?",
                (name, diff)).fetchone()
            if cur is not None and cur[1] <= time_ms:
                improved, row_id = False, cur[0]
            else:
                if cur is not None:
                    self.conn.execute(
                        "UPDATE leaderboard SET time_ms=?, ts=? WHERE id=?",
                        (time_ms, int(time.time()), cur[0]))
                    row_id = cur[0]
                else:
                    self.conn.execute(
                        "INSERT INTO leaderboard(name,difficulty,time_ms,ts) "
                        "VALUES(?,?,?,?)",
                        (name, diff, time_ms, int(time.time())))
                    row_id = self.conn.execute(
                        "SELECT last_insert_rowid()").fetchone()[0]
                self.conn.commit()
                improved = True
            # rank of the player's current best time on this difficulty
            best = self.conn.execute(
                "SELECT time_ms, id FROM leaderboard "
                "WHERE name=? AND difficulty=?", (name, diff)).fetchone()
            best_ms, best_id = best[0], best[1]
            below = self.conn.execute(
                "SELECT COUNT(*) FROM leaderboard WHERE difficulty=? "
                "AND time_ms < ?", (diff, best_ms)).fetchone()[0]
            tied = self.conn.execute(
                "SELECT COUNT(*) FROM leaderboard WHERE difficulty=? "
                "AND time_ms = ? AND id <= ?",
                (diff, best_ms, best_id)).fetchone()[0]
            return improved, below + tied

    def top_scores(self, diff, limit):
        """Return (rank, name, difficulty, time_ms, ts), best first.  Rank is
        the per-difficulty position (1-based) recomputed from the ordering."""
        with self.lock:
            if diff is None:
                rows = self.conn.execute(
                    "SELECT name, difficulty, time_ms, ts FROM leaderboard "
                    "ORDER BY difficulty, time_ms, id LIMIT ?",
                    (limit,)).fetchall()
            else:
                rows = self.conn.execute(
                    "SELECT name, difficulty, time_ms, ts FROM leaderboard "
                    "WHERE difficulty=? ORDER BY time_ms, id LIMIT ?",
                    (diff, limit)).fetchall()
        out = []
        counts = {}
        for name, d, ms, ts in rows:
            counts[d] = counts.get(d, 0) + 1
            out.append((counts[d], name, d, ms, ts))
        return out

    def close(self):
        with self.lock:
            self.conn.close()


class ClientHub:
    """Thread-safe registry of connected telemetry clients."""

    def __init__(self, db):
        self.db = db
        self.lock = threading.Lock()
        self.clients = {}   # addr -> {"conn": socket, "last": ts}

    def add(self, addr, conn):
        with self.lock:
            now = int(time.time())
            self.clients[addr] = {"conn": conn, "last": now, "seeds": 0,
                                  "outcomes": 0, "user": None, "nonce": None,
                                  "nonce_ts": 0, "fails": 0, "authed": False}
        self.db.upsert_client(addr, now)

    def get(self, addr):
        """Snapshot of one client's state (or None)."""
        with self.lock:
            cl = self.clients.get(addr)
            return dict(cl) if cl else None

    def is_authed(self, addr):
        """True if the connection passed the solver-auth handshake."""
        with self.lock:
            cl = self.clients.get(addr)
            return bool(cl is not None and cl.get("authed"))

    def auth_begin(self, addr, user):
        """Issue a fresh challenge nonce for a connection.  Returns the hex
        nonce, or None if the connection is gone."""
        with self.lock:
            cl = self.clients.get(addr)
            if cl is None:
                return None
            cl["user"] = user
            cl["nonce"] = os.urandom(16).hex()
            cl["nonce_ts"] = int(time.time())
            return cl["nonce"]

    def auth_resolve(self, addr, digest_hex, expected_hex):
        """Check a challenge response (expected_hex precomputed by the caller
        from the nonce).  Returns (ok, fails): on a match the connection is
        authenticated; on a mismatch the failure counter is bumped.  The
        nonce is consumed either way."""
        with self.lock:
            cl = self.clients.get(addr)
            if cl is None:
                return False, 0
            nonce = cl.get("nonce")
            nonce_ts = cl.get("nonce_ts", 0)
            if nonce is None or int(time.time()) - nonce_ts > NONCE_TTL:
                cl["fails"] = cl.get("fails", 0) + 1
                return False, cl["fails"]
            ok = hmac.compare_digest(digest_hex.lower(), expected_hex)
            cl["nonce"] = None
            if ok:
                cl["authed"] = True
                cl["fails"] = 0
            else:
                cl["fails"] = cl.get("fails", 0) + 1
            return ok, cl["fails"]

    def remove(self, addr):
        with self.lock:
            self.clients.pop(addr, None)
        self.db.upsert_client(addr, int(time.time()), active=False)

    def count(self):
        with self.lock:
            return len(self.clients)

    def send_to(self, addr, line):
        """Send one line to a single client.  Returns True if delivered.

        Dead sockets are dropped (and their DB row marked inactive).  Does
        not throw, so callers can stream many lines without exception
        handling."""
        present = False
        sent = False
        seeds = outcomes = 0
        with self.lock:
            cl = self.clients.get(addr)
            if cl is not None:
                present = True
                try:
                    cl["conn"].sendall((line + "\n").encode("ascii"))
                    cl["last"] = int(time.time())
                    if line.startswith("seed "):
                        cl["seeds"] += 1
                    elif line.startswith("outcome "):
                        cl["outcomes"] += 1
                    sent = True
                    # Capture the counters under the same lock that guards the
                    # increments: the producer's broadcast() and a request
                    # worker's send_to() can mutate this client's counters
                    # concurrently, so reading them after the lock could write
                    # a stale (one-behind) value to the DB.
                    seeds, outcomes = cl["seeds"], cl["outcomes"]
                except OSError:
                    pass
        if present and not sent:
            self.remove(addr)
        elif sent:
            self.db.client_touch(addr, seeds, outcomes)
        return sent

    def broadcast(self, line):
        """Send a line to every client.  Drops dead sockets.  Returns the
        number of clients that received it."""
        sent = 0
        dead = []
        touched = []   # (addr, seeds, outcomes) captured under the lock
        with self.lock:
            for addr, cl in self.clients.items():
                try:
                    cl["conn"].sendall((line + "\n").encode("ascii"))
                    cl["last"] = int(time.time())
                    if line.startswith("seed "):
                        cl["seeds"] += 1
                    elif line.startswith("outcome "):
                        cl["outcomes"] += 1
                    sent += 1
                    # Read the counters before the lock is released so this
                    # batch reflects exactly the sends performed above (see
                    # the note in send_to), and touch only the recipients.
                    touched.append((addr, cl["seeds"], cl["outcomes"]))
                except OSError:
                    dead.append(addr)
        for addr in dead:
            self.remove(addr)
        self.db.client_touch_many(touched)
        return sent


class RequestWorkers:
    """Per-client request processors.

    Each client that sends a `reqseed`/`reqbatch`/`requntil` line gets its
    own worker thread consuming that client's requests FIFO.  Requests from
    different clients therefore never wait on each other: a long `requntil`
    on one connection cannot stall the requests of another (the GIL still
    shares CPU between workers, but nothing queues behind a running
    request).

    The worker lives as long as the connection; dropping the connection
    pushes a None sentinel that makes it exit after draining its queue.
    """

    def __init__(self, server):
        self.server = server
        self.lock = threading.Lock()
        self.workers = {}   # addr_s -> {"q": Queue, "t": Thread}

    def enqueue(self, addr_s, line):
        """Dispatch one request line to addr_s's worker (creating it)."""
        with self.lock:
            w = self.workers.get(addr_s)
            if w is None:
                q = queue.Queue()
                t = threading.Thread(target=self._run, args=(addr_s, q),
                                     daemon=True)
                t.start()
                w = {"q": q, "t": t}
                self.workers[addr_s] = w
            w["q"].put(line)

    def drop(self, addr_s):
        """Called when the client connection closes; retires its worker."""
        with self.lock:
            w = self.workers.pop(addr_s, None)
        if w is not None:
            w["q"].put(None)

    def _run(self, addr_s, q):
        while True:
            line = q.get()          # blocks; sentinel None retires us
            if line is None or self.server.stop_event.is_set():
                return
            handle_request(self.server, addr_s, line)


class AdmissionGate:
    """Fair FIFO admission control for heavy (CPU-expensive) requests.

    Heavy requests acquire the gate before they start computing and release
    it when they finish.  Only --max-concurrent heavy requests run at once,
    and because the GIL caps Python CPU at ~1 core, that means the first-
    arrived heavy request runs at ~full speed while later ones wait their
    turn instead of all slowing each other by the total number of requests.

    Fairness is enforced with ticket numbers: a release admits the longest-
    waiting ticket, so a flood of heavy requests is served in arrival order
    and no client is starved.
    """

    def __init__(self, max_concurrent=1):
        self.max_concurrent = max(1, max_concurrent)
        self.cond = threading.Condition()
        self.free = self.max_concurrent   # slots currently available
        self.head = 0                     # next ticket in line to be admitted
        self.next_ticket = 0              # tickets handed out

    def acquire(self):
        with self.cond:
            ticket = self.next_ticket
            self.next_ticket += 1
            while self.free == 0 or ticket != self.head:
                self.cond.wait()
            self.free -= 1
            self.head += 1

    def release(self):
        with self.cond:
            self.free += 1
            self.cond.notify_all()


def conn_thread(server, conn, addr):
    """Read `metric` lines from one client connection until EOF."""
    addr_s = "%s:%d" % addr
    server.hub.add(addr_s, conn)
    line_buf = b""
    try:
        while not server.stop_event.is_set():
            chunk = conn.recv(4096)
            if not chunk:
                break
            line_buf += chunk
            if len(line_buf) > MAX_LINE and b"\n" not in line_buf:
                # single oversized line: would grow line_buf without bound
                print("  conn: %s oversized line, closing" % addr_s, flush=True)
                break
            while b"\n" in line_buf:
                raw, line_buf = line_buf.split(b"\n", 1)
                text = raw.decode("ascii", "replace").strip()
                if not text:
                    continue
                if text.startswith("metric "):
                    server.db.record_metric(int(time.time()),
                                            addr_s, text)
                elif text.startswith("auth "):
                    handle_auth(server, addr_s, conn, text)
                elif text.startswith("authresp "):
                    handle_authresp(server, addr_s, conn, text)
                elif text.startswith("lbscore "):
                    handle_lbscore(server, addr_s, text)
                elif text.startswith("lbtop "):
                    handle_lbtop(server, addr_s, text)
                elif text.startswith("reqseed ") or \
                        text.startswith("reqbatch ") or \
                        text.startswith("requntil "):
                    # the solver (on-demand request system) is gated: deny
                    # when disabled or before the client authenticates.
                    if not server.solver_enabled or \
                            not server.hub.is_authed(addr_s):
                        server.hub.send_to(addr_s, "reqdenied")
                    else:
                        server.req_workers.enqueue(addr_s, text)
    except OSError:
        pass
    finally:
        server.hub.remove(addr_s)
        server.req_workers.drop(addr_s)
        try:
            conn.close()
        except OSError:
            pass


def handle_auth(server, addr_s, conn, line):
    """Start the solver-auth challenge for a connection."""
    toks = line.split()
    if len(toks) < 2 or not server.solver_enabled:
        server.hub.send_to(addr_s, "autherr")
        return
    user = toks[1]
    if not hmac.compare_digest(user, server.solver_user):
        server.hub.send_to(addr_s, "autherr")
        print("  auth: unknown user %r from %s" % (user, addr_s), flush=True)
        return
    nonce = server.hub.auth_begin(addr_s, user)
    if nonce is None:
        server.hub.send_to(addr_s, "autherr")
        return
    server.hub.send_to(addr_s, "authchal %s" % nonce)


def handle_authresp(server, addr_s, conn, line):
    """Check a challenge response; authenticate or bump the failure count."""
    toks = line.split()
    if len(toks) < 2:
        server.hub.send_to(addr_s, "autherr")
        return
    cl = server.hub.get(addr_s)
    nonce = cl.get("nonce") if cl else None
    user = cl.get("user") if cl else None
    if nonce is None:
        server.hub.send_to(addr_s, "autherr")
        return
    msg = ("ms-auth:" + nonce).encode("ascii")
    expected = hmac.new(server.solver_pass.encode("utf-8"), msg,
                        hashlib.sha256).hexdigest()
    ok, fails = server.hub.auth_resolve(addr_s, toks[1], expected)
    if ok:
        server.hub.send_to(addr_s, "authok")
        print("  auth: ok user=%s from %s" % (user, addr_s), flush=True)
    else:
        server.hub.send_to(addr_s, "autherr")
        print("  auth: FAILED user=%s from %s (fails=%d)"
              % (user, addr_s, fails), flush=True)
        if fails >= MAX_AUTH_FAILS:
            # lockout: drop the connection (caught by conn_thread)
            raise OSError("too many auth failures")


def handle_lbscore(server, addr_s, line):
    """Record a leaderboard best time (rate-limited per source IP)."""
    toks = line.split()
    if len(toks) != 4:
        return
    name, diff = toks[1], toks[2].lower()
    if not NAME_RE.match(name) or diff not in DIFFS:
        return
    try:
        ms = int(toks[3])
    except ValueError:
        return
    if ms < 0 or ms > 3600000:
        return
    ip = addr_s.rsplit(":", 1)[0]
    now = time.monotonic()
    with server.lb_lock:
        hist = server.lb_hist.setdefault(ip, [])
        hist[:] = [t for t in hist if now - t < LB_WINDOW]
        if len(hist) >= LB_MAX:
            server.hub.send_to(addr_s, "lbdenied")
            return
        hist.append(now)
        # bound the tracking dict: first drop IPs whose window went quiet,
        # then evict the least-recently-active remainders if still over cap.
        if len(server.lb_hist) > LB_MAX_IPS:
            server.lb_hist = {
                k: v for k, v in server.lb_hist.items()
                if any(now - t < LB_WINDOW for t in v)
            }
        while len(server.lb_hist) > LB_MAX_IPS:
            k = min(server.lb_hist, key=lambda kk: server.lb_hist[kk][-1])
            del server.lb_hist[k]
    improved, rank = server.db.record_score(name, diff, ms)
    if improved:
        server.hub.send_to(addr_s,
                           "lbstored %d %s %s %d" % (rank, diff, name, ms))
    else:
        server.hub.send_to(addr_s, "lbnotop")


def handle_lbtop(server, addr_s, line):
    """Stream the top leaderboard times for one difficulty or all of them."""
    toks = line.split()
    count = 10
    diff = None
    if len(toks) >= 3 and toks[1].lower() in DIFFS:
        diff = toks[1].lower()
        try:
            count = int(toks[2])
        except ValueError:
            return
    elif len(toks) >= 2:
        try:
            count = int(toks[1])
        except ValueError:
            return
    if count < 1 or count > 100:
        return
    entries = server.db.top_scores(diff, count)
    if diff is None:
        server.hub.send_to(addr_s, "lbtop %d" % len(entries))
    else:
        server.hub.send_to(addr_s, "lbtop %s %d" % (diff, len(entries)))
    for rank, name, d, ms, ts in entries:
        server.hub.send_to(addr_s,
                           "lbentry %d %s %s %d %d" % (rank, d, name, ms, ts))
    server.hub.send_to(addr_s, "lbdone")


def accept_loop(server):
    while not server.stop_event.is_set():
        try:
            conn, addr = server.listen_sock.accept()
        except OSError:
            break
        t = threading.Thread(target=conn_thread, args=(server, conn, addr),
                             daemon=True)
        t.start()


def simulate_game(server, rng, diff, seed, requester=None):
    """Play one simulated game and record it.  Returns the DB row dict.

    The board layout is fully determined by (diff, seed); `rng` only drives
    the solver's guess tie-breaks.  Heavy requests are serialized in FIFO
    order by the server's admission gate (see the module docstring's
    "Admission gate" note) so the GIL's single core of Python CPU is not
    split among every concurrent request."""
    board = SimBoard()
    board.new(diff, seed)
    client = SimClient(sim=board, seed=seed)
    t0 = time.perf_counter()
    res = play_game(client, diff, SOLVER_STRATEGY, rng)
    wall_ms = int((time.perf_counter() - t0) * 1000)
    g = {
        "ts": int(time.time()),
        "difficulty": diff,
        "seed": seed,
        "won": res["win"],
        "moves": res["moves"],
        "time_ms": int(round(res["time"] * 1000)),
        "guesses": res["guesses"],
        "chords": res["chords"],
        "flags": res["flags"],
        "deduce_batches": res["deduce_batches"],
        "frontier": res["frontier"],
        "wall_ms": wall_ms,
        "requester": requester,
    }
    server.db.record_game(g)
    return g


def outcome_line(diff, seed, g):
    return "outcome %s %d %d %d %d %d" % (
        diff, seed, int(g["won"]), g["moves"], g["time_ms"], g["guesses"])


def produce(server, rng):
    """Play simulated games and broadcast seed/outcome lines to all clients."""
    db = server.db
    hub = server.hub
    while not server.stop_event.is_set():
        # backpressure: only simulate while at least one client is connected
        while hub.count() == 0 and not server.stop_event.is_set():
            server.stop_event.wait(0.25)
        if server.stop_event.is_set():
            break

        diff = rng.choice(server.diffs)
        seed = rng.randrange(0, 1 << 63)
        g = simulate_game(server, rng, diff, seed)

        hub.broadcast("seed %s %d" % (diff, seed))
        hub.broadcast(outcome_line(diff, seed, g))

        if server.rate > 0:
            server.stop_event.wait(1.0 / server.rate)


def handle_request(server, addr_s, line):
    """Play one seed/batch/until request for a single client.

    Each requested game is streamed only to the requesting client (marked
    with a `reqgame` line so the client can distinguish it from the broadcast
    stream) and recorded in sim_games with requester set.  Heavy requests
    (estimated CPU >= HEAVY_CPU_SECONDS) are admitted in FIFO order through
    the server's AdmissionGate: they send a `reqwait` line when queued and
    only start computing once all earlier heavy requests have finished, so a
    flood of heavy requests cannot slow each other down.  The solver is
    protected: requests only run when credentials are configured and the
    connection authenticated (conn_thread already gates this; the check here
    is a second line of defence for any code path that enqueues directly)."""
    if not getattr(server, "solver_enabled", False) or \
            not server.hub.is_authed(addr_s):
        server.hub.send_to(addr_s, "reqdenied")
        return
    hub = server.hub
    toks = line.split()
    cmd = toks[0].lower()
    diff = None
    seed = None
    count = 1
    until = False
    if cmd == "reqseed" and len(toks) >= 3:
        diff = toks[1].lower()
        try:
            seed = int(toks[2])
            if len(toks) >= 4:
                count = int(toks[3])
        except ValueError:
            return
    elif cmd == "reqbatch" and len(toks) >= 3:
        diff = toks[1].lower()
        try:
            count = int(toks[2])
        except ValueError:
            return
    elif cmd == "requntil" and len(toks) >= 3:
        diff = toks[1].lower()
        until = True
        try:
            seed = int(toks[2])
            if len(toks) >= 4:
                count = int(toks[3])
        except ValueError:
            return
    else:
        return

    if diff not in DIFFS or count < 1:
        return
    count = min(count, server.max_request)

    heavy = count * GAME_CPU_SECONDS.get(diff, 0) >= HEAVY_CPU_SECONDS
    if heavy:
        hub.send_to(addr_s, "reqwait %s %d" % (diff,
                                               seed if seed is not None else count))
        server.gate.acquire()
    try:
        batch_rng = random.Random(seed) if seed is not None else random.Random()
        played = 0
        loss = None          # (won, moves, time_ms, guesses) of the first loss
        for run in range(count):
            s = seed if seed is not None else batch_rng.randrange(0, 1 << 63)
            # one replay of a seed is deterministic; repeated runs (multi-sim)
            # vary the solver's tie-break randomness so the same board can
            # show its range of outcomes.
            decision = random.Random(s ^ (run << 32))
            if not hub.send_to(addr_s, "reqgame %s %d" % (diff, s)):
                break
            g = simulate_game(server, decision, diff, s, requester=addr_s)
            # NOTE: simulate_game() + record_game() run even if the stream
            # dies right after (the next send_to below returns False).  This
            # is intentional: the game was legitimately played and must be
            # recorded with this requester even if delivery failed.  Don't
            # reorder this to "check liveness before computing" -- a dead
            # socket is only detectable on the next send anyway, and moving
            # the record would silently drop DB rows.
            if not hub.send_to(addr_s, "seed %s %d" % (diff, s)):
                break
            if not hub.send_to(addr_s, outcome_line(diff, s, g)):
                break
            played += 1
            if until and not g["won"]:
                loss = (g["won"], g["moves"], g["time_ms"], g["guesses"])
                break
        if until:
            if loss is not None:
                hub.send_to(addr_s, "lossfound %s %d %d %d %d %d %d" % (
                    diff, seed, played - 1, loss[0], loss[1], loss[2],
                    loss[3]))
            else:
                hub.send_to(addr_s, "noloss %s %d %d" % (diff, seed, played))
        hub.send_to(addr_s, "reqdone %s %d" % (diff, played))
    finally:
        if heavy:
            server.gate.release()


def resolve_solver(args):
    """Resolve solver credentials: explicit args > --solver-config > env.

    Returns (user, pass) with None for either when unset."""
    user, pw = args.solver_user, args.solver_pass
    if args.solver_config:
        with open(args.solver_config, "r", encoding="utf-8") as fh:
            data = json.load(fh)
        user = user or data.get("user")
        pw = pw or data.get("pass")
    user = user or os.environ.get("MS_SOLVER_USER")
    pw = pw or os.environ.get("MS_SOLVER_PASS")
    return user, pw


def run(args):
    solver_user, solver_pass = resolve_solver(args)
    solver_enabled = bool(solver_user and solver_pass)
    server = argparse.Namespace(
        stop_event=threading.Event(),
        db=Database(args.db),
        hub=None,
        listen_sock=None,
        diffs=(args.difficulty.split(",") if args.difficulty != "all"
               else DIFFS),
        rate=args.rate,
        max_request=args.max_request,
        gate=AdmissionGate(args.max_concurrent),
        req_workers=None,
        solver_user=solver_user,
        solver_pass=solver_pass,
        solver_enabled=solver_enabled,
        lb_lock=threading.Lock(),
        lb_hist={},
    )
    server.hub = ClientHub(server.db)
    server.req_workers = RequestWorkers(server)

    rng = random.Random(args.seed if args.seed is not None else None)
    for d in server.diffs:
        if d not in DIFFS:
            print("unknown difficulty %r (use all|beginner|intermediate|"
                  "expert)" % d, file=sys.stderr)
            return 2

    ls = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    ls.bind((args.host, args.port))
    ls.listen(128)          # match the box's net.core.somaxconn (4096)
    server.listen_sock = ls

    print("ms_server listening on %s:%d  (db=%s  rate=%.1f g/s  "
          "max-concurrent=%d  solver=%s)"
          % (args.host, args.port, args.db, args.rate,
             args.max_concurrent,
             "protected" if solver_enabled else "disabled"), flush=True)
    try:
        acc = threading.Thread(target=accept_loop, args=(server,), daemon=True)
        acc.start()
        prod = threading.Thread(target=produce, args=(server, rng), daemon=True)
        prod.start()
        while not server.stop_event.is_set():
            time.sleep(1.0)
            g, m, c = server.db.counts()
            print("  games=%d wins=%d metrics=%d clients=%d"
                  % (g[0], g[1] or 0, m[0], c[0]), flush=True)
    except KeyboardInterrupt:
        print("\nshutting down...", flush=True)
    finally:
        server.stop_event.set()
        try:
            ls.close()
        except OSError:
            pass
        time.sleep(0.2)
        server.db.close()
    return 0


def selfcheck():
    """No-network sanity pass: run simulated games through the solver."""
    rng = random.Random(42)
    ok = True
    for diff in DIFFS:
        wins = 0
        games = 0
        moves_sum = 0
        for _ in range(20):
            seed = rng.randrange(0, 1 << 63)
            board = SimBoard()
            board.new(diff, seed)
            client = SimClient(sim=board, seed=seed)
            res = play_game(client, diff, SOLVER_STRATEGY, rng)
            games += 1
            wins += 1 if res["win"] else 0
            moves_sum += res["moves"]
            if res["moves"] < 1:
                print("  FAIL %s: no moves" % diff)
                ok = False
            b = client.board()
            if len(b) != board.rows or any(len(r) != board.cols for r in b):
                print("  FAIL %s: board size mismatch" % diff)
                ok = False
        print("  %-12s games=%-3d wins=%-3d avg_moves=%.1f%s"
              % (diff, games, wins, moves_sum / max(1, games),
                 "  OK" if wins > 0 else ""))
        if wins == 0:
            ok = False
    print("selfcheck:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


def main():
    ap = argparse.ArgumentParser(description="Minesweeper simulation server")
    ap.add_argument("--host", default="0.0.0.0")
    ap.add_argument("--port", type=int, default=28571)
    ap.add_argument("--db", default=os.path.join(HERE, "data", "sim.db"))
    ap.add_argument("--rate", type=float, default=5.0,
                    help="simulated games per second (0 = as fast as possible)")
    ap.add_argument("--max-request", type=int, default=10000,
                    help="hard cap on games a single request may ask for")
    ap.add_argument("--max-concurrent", type=int, default=1,
                    help="how many heavy requests may compute at once.  The "
                         "GIL caps total Python CPU at ~1 core, so with "
                         "1 (default) the first-arrived heavy request runs "
                         "at ~full speed and later ones are served in FIFO "
                         "order (they get a reqwait line); 2+ lets that many "
                         "run concurrently at reduced speed.  Light requests "
                         "always bypass the gate.")
    ap.add_argument("--difficulty", default="all")
    ap.add_argument("--seed", type=int, default=None)
    ap.add_argument("--solver-user", default=None,
                    help="username required to use the solver (reqseed/"
                         "reqbatch/requntil); without it the solver is "
                         "disabled and every request is denied")
    ap.add_argument("--solver-pass", default=None,
                    help="password for --solver-user (never sent over the "
                         "wire; the client answers a per-connection "
                         "HMAC-SHA256 challenge instead)")
    ap.add_argument("--solver-config", default=None,
                    help="JSON file with 'user' and 'pass' keys for the "
                         "solver gate (args override the file, the file "
                         "overrides MS_SOLVER_USER/MS_SOLVER_PASS env)")
    ap.add_argument("--selfcheck", action="store_true")
    args = ap.parse_args()
    if args.selfcheck:
        return selfcheck()
    # The box has 4 cores and the GIL caps Python compute at ~1; a longer
    # switch interval cuts preemption/context-switch overhead when the
    # producer, request workers and gate-guarded heavy requests share the
    # CPU, at the cost of coarser fairness between threads.
    try:
        sys.setswitchinterval(0.02)
    except (ValueError, AttributeError):
        pass
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
