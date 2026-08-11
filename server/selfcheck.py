"""selfcheck.py - end-to-end test of the simulation server on localhost.

Run from the server/ directory:

    python selfcheck.py

Steps:
  1. sim sanity: the same simulated games ms_server runs (via ms_server.selfcheck).
  2. live server without solver creds: stream + metrics work, leaderboard
     submit/query works, and solver requests are DENIED (reqdenied).
  3. live server WITH solver creds: solver requests are denied before auth,
     the HMAC-SHA256 challenge handshake rejects a wrong user / wrong
     password, locks out after 5 failures, and reqseed/reqbatch succeed
     once authenticated.
  4. verifies the SQLite contents (games, metrics, requests, leaderboard).

Returns 0 on success.
"""

import hashlib
import hmac
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))

SOLVER_USER = "testuser"
SOLVER_PASS = "test-secret-123"


def find_free_port(base=29410):
    for p in range(base, base + 200):
        s = socket.socket()
        try:
            s.bind(("127.0.0.1", p))
            s.close()
            return p
        except OSError:
            s.close()
    raise RuntimeError("no free port near %d" % base)


class LineReader(object):
    """Read newline-terminated lines from a socket without dropping any bytes
    that arrive past the first newline in a recv() chunk."""

    def __init__(self, sock, timeout=10.0):
        self.sock = sock
        self.timeout = timeout
        self.buf = b""

    def read_line(self):
        self.sock.settimeout(self.timeout)
        while b"\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("server closed connection")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode("ascii", "replace").strip()


def spawn_server(port, db, extra=()):
    """Start ms_server.py on a temp port/db; return (proc, connected sock)."""
    proc = subprocess.Popen(
        [sys.executable, os.path.join(HERE, "ms_server.py"),
         "--host", "127.0.0.1", "--port", str(port), "--db", db,
         "--rate", "50", "--seed", "99"] + list(extra),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sock = None
    deadline = time.time() + 15
    while time.time() < deadline:
        try:
            sock = socket.create_connection(("127.0.0.1", port), timeout=1)
            break
        except OSError:
            if proc.poll() is not None:
                raise RuntimeError(
                    "server exited early with code %r" % proc.returncode)
            time.sleep(0.2)
    if sock is None:
        proc.kill()
        raise RuntimeError("server did not open port %d" % port)
    return proc, sock


def drain_broadcast(reader):
    """Read a broadcast seed/outcome pair from the stream."""
    seen_seed = seen_outcome = None
    deadline = time.time() + 15
    while time.time() < deadline:
        parts = reader.read_line().split()
        if not parts:
            continue
        if parts[0] == "seed" and seen_seed is None:
            seen_seed = parts
        elif parts[0] == "outcome":
            seen_outcome = parts
            if seen_seed is not None and parts[1:3] == seen_seed[1:3]:
                break
    if seen_seed is None or seen_outcome is None:
        raise RuntimeError("no seed/outcome pair seen; seed=%r outcome=%r"
                           % (seen_seed, seen_outcome))
    if seen_outcome[1:3] != seen_seed[1:3]:
        raise RuntimeError("outcome seed/diff mismatch: %r %r"
                           % (seen_seed, seen_outcome))
    if len(seen_outcome) != 7:
        raise RuntimeError("malformed outcome line: %r" % seen_outcome)
    return seen_seed, seen_outcome


def auth_handshake(sock, reader, user, password):
    """Complete one solver-auth handshake; returns True on authok."""
    sock.sendall(("auth %s\n" % user).encode("ascii"))
    deadline = time.time() + 10
    while time.time() < deadline:
        parts = reader.read_line().split()
        if not parts:
            continue
        if parts[0] == "authchal":
            digest = hmac.new(password.encode("utf-8"),
                              ("ms-auth:" + parts[1]).encode("ascii"),
                              hashlib.sha256).hexdigest()
            sock.sendall(("authresp %s\n" % digest).encode("ascii"))
        elif parts[0] == "authok":
            return True
        elif parts[0] == "autherr":
            return False
    raise RuntimeError("timed out during auth handshake")


def collect_requests(reader, sock, req_line, timeout=15):
    """Send a seed/batch request and collect the finished games."""
    requested = []
    sock.sendall(req_line.encode("ascii") + b"\n")
    pending = None
    done = None
    deadline = time.time() + timeout
    while time.time() < deadline:
        parts = reader.read_line().split()
        if not parts:
            continue
        if parts[0] == "reqgame":
            pending = (parts[1], int(parts[2]))
        elif parts[0] == "seed":
            pass
        elif parts[0] == "outcome" and pending is not None:
            if (parts[1], int(parts[2])) == pending:
                requested.append((parts[1], int(parts[2]), int(parts[3]),
                                  int(parts[4]), int(parts[5]), int(parts[6])))
                pending = None
        elif parts[0] == "reqdone":
            done = (parts[1], int(parts[2]))
            break
    return done, requested


def expect(reader, prefix, what):
    """Read lines until one starts with `prefix`; return its parts."""
    deadline = time.time() + 10
    while time.time() < deadline:
        parts = reader.read_line().split()
        if parts and parts[0] == prefix:
            return parts
    raise RuntimeError("no %s line seen" % what)


def expect_any(reader, prefixes, what):
    """Read lines until one starts with any of `prefixes`; return its parts."""
    deadline = time.time() + 10
    while time.time() < deadline:
        parts = reader.read_line().split()
        if parts and parts[0] in prefixes:
            return parts
    raise RuntimeError("no %s line seen" % what)


def main():
    sys.path.insert(0, HERE)
    import ms_server

    print("[1/5] solver self-check over SimBoard")
    if ms_server.selfcheck() != 0:
        print("FAILED: solver self-check")
        return 1

    port = find_free_port()
    with tempfile.TemporaryDirectory() as tmp:
        db = os.path.join(tmp, "sim.db")

        # ---------- server without solver creds ----------
        print("[2/5] live server (no solver creds): stream, leaderboard, "
              "solver denied")
        proc, sock = spawn_server(port, db)
        try:
            reader = LineReader(sock)
            seed, outcome = drain_broadcast(reader)
            print("    got seed %s %s  ->  outcome won=%s moves=%s "
                  "time_ms=%s guesses=%s" % (
                      outcome[1], outcome[2], outcome[3], outcome[4],
                      outcome[5], outcome[6]))

            sock.sendall(b"metric start diff=beginner seed=1 seeded=1 t=1\n")
            sock.sendall(b"metric win diff=beginner seed=1 seeded=1 time=42 "
                         b"clicks=10 latency=123 t=2\n")
            time.sleep(0.5)

            # solver disabled -> denied, even though no auth attempted
            sock.sendall(b"reqseed beginner 12345\n")
            denied = expect(reader, "reqdenied", "reqdenied")
            print("    reqseed without creds -> %s" % " ".join(denied))

            # leaderboard: submit + improve + slower (no rank change)
            sock.sendall(b"lbscore Player1 beginner 45000\n")
            parts = expect(reader, "lbstored", "lbstored")
            assert parts[1:5] == ["1", "beginner", "Player1", "45000"], parts
            sock.sendall(b"lbscore Player1 beginner 40000\n")
            parts = expect(reader, "lbstored", "lbstored")
            assert parts[1:5] == ["1", "beginner", "Player1", "40000"], parts
            sock.sendall(b"lbscore Player1 beginner 42000\n")
            expect(reader, "lbnotop", "lbnotop")
            sock.sendall(b"lbscore Player2 intermediate 120000\n")
            parts = expect(reader, "lbstored", "lbstored")
            assert parts[1:5] == ["1", "intermediate", "Player2", "120000"], parts
            print("    lbscore submit/improve/skip -> ok")

            # invalid names are ignored (no reply)
            sock.sendall(b"lbscore 'bad name!' beginner 1000\n")
            sock.sendall(b"lbtop 10\n")
            parts = expect(reader, "lbtop", "lbtop header")
            assert int(parts[1]) == 2, parts
            got = []
            while True:
                parts = expect_any(reader, ("lbentry", "lbdone"),
                                   "lbentry/lbdone")
                if parts[0] == "lbentry":
                    got.append(parts)
                else:
                    break
            assert len(got) == 2, got
            print("    lbtop 10 -> %d entries" % len(got))
            sock.close()
        finally:
            proc.terminate()
            proc.wait(timeout=5)

        # ---------- server WITH solver creds ----------
        print("[3/5] live server (solver creds): auth gate + requests")
        port2 = find_free_port()
        proc2, sock2 = spawn_server(
            port2, db, ["--solver-user", SOLVER_USER,
                        "--solver-pass", SOLVER_PASS])
        try:
            reader2 = LineReader(sock2)

            # unauthenticated request denied
            sock2.sendall(b"reqseed beginner 12345\n")
            expect(reader2, "reqdenied", "reqdenied")
            print("    reqseed before auth -> reqdenied")

            # wrong user
            sock2.sendall(b"auth wronguser\n")
            expect(reader2, "autherr", "autherr")
            print("    auth wrong user -> autherr")

            # wrong password on a real challenge
            sock2.sendall(b"auth %s\n" % SOLVER_USER.encode("ascii"))
            chal = expect(reader2, "authchal", "authchal")
            bad = hmac.new(b"wrong-password",
                           ("ms-auth:" + chal[1]).encode("ascii"),
                           hashlib.sha256).hexdigest()
            sock2.sendall(("authresp %s\n" % bad).encode("ascii"))
            expect(reader2, "autherr", "autherr")
            print("    auth wrong password -> autherr")

            # correct handshake
            assert auth_handshake(sock2, reader2, SOLVER_USER, SOLVER_PASS)
            print("    auth handshake -> authok")

            # requests now work
            done, requested = collect_requests(reader2, sock2,
                                               "reqseed beginner 12345")
            assert done == ("beginner", 1), done
            assert len(requested) == 1 and requested[0][1] == 12345, requested
            print("    reqseed beginner 12345 -> won=%s moves=%s guesses=%s"
                  % (requested[0][2], requested[0][3], requested[0][5]))
            done, requested = collect_requests(reader2, sock2,
                                               "reqbatch beginner 5")
            assert done == ("beginner", 5), done
            assert len(requested) == 5, requested
            print("    reqbatch beginner 5 -> %d games" % len(requested))

            # lockout: 5 wrong passwords closes the connection
            print("[4/5] auth lockout (5 failures closes the connection)")
            sock3 = socket.create_connection(("127.0.0.1", port2), timeout=10)
            reader3 = LineReader(sock3)
            locked = False
            try:
                for _ in range(5):
                    sock3.sendall(b"auth %s\n" % SOLVER_USER.encode("ascii"))
                    chal = expect(reader3, "authchal", "authchal")
                    bad = hmac.new(b"x", ("ms-auth:" + chal[1]).encode("ascii"),
                                   hashlib.sha256).hexdigest()
                    sock3.sendall(("authresp %s\n" % bad).encode("ascii"))
                    expect(reader3, "autherr", "autherr")
                reader3.read_line()     # should raise: connection closed
            except ConnectionError:
                locked = True
            assert locked, "server did not close after 5 auth failures"
            print("    connection closed after 5 failures -> ok")
            sock2.close()
        finally:
            proc2.terminate()
            proc2.wait(timeout=5)

        print("[5/5] verifying SQLite contents")
        time.sleep(0.5)
        con = sqlite3.connect(db)
        games = con.execute("SELECT COUNT(*) FROM sim_games").fetchone()[0]
        wins = con.execute(
            "SELECT COUNT(*) FROM sim_games WHERE won=1").fetchone()[0]
        metrics = con.execute(
            "SELECT line FROM client_metrics ORDER BY id").fetchall()
        req_rows = con.execute(
            "SELECT COUNT(*) FROM sim_games WHERE requester IS NOT NULL"
            ).fetchone()[0]
        replay = con.execute(
            "SELECT won FROM sim_games WHERE seed=12345 "
            "AND requester IS NOT NULL").fetchall()
        lb = con.execute(
            "SELECT name, difficulty, time_ms FROM leaderboard "
            "ORDER BY difficulty, time_ms").fetchall()
        con.close()
        if games < 2:
            raise RuntimeError("expected sim_games rows, got %d" % games)
        if wins < 1:
            raise RuntimeError("expected some winning sim games")
        if not any("metric win" in m[0] for m in metrics):
            raise RuntimeError("metric win line not recorded")
        if req_rows != 6:
            raise RuntimeError("expected 6 requested games (1 seed + 5 "
                               "batch), got %d" % req_rows)
        if len(replay) != 1:
            raise RuntimeError("seed 12345 request not recorded: %r" % replay)
        if lb != [("Player1", "beginner", 40000),
                  ("Player2", "intermediate", 120000)]:
            raise RuntimeError("leaderboard rows mismatch: %r" % lb)
        print("    sim_games=%d (wins=%d), client_metrics=%d rows, "
              "requested=%d (seed 12345 won=%s), leaderboard=%d rows"
              % (games, wins, len(metrics), req_rows, replay[0][0], len(lb)))

    print("selfcheck: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
