"""ms_analyze.py - win/loss analysis client for the telemetry server.

Uses the server's seed-request system to run controlled batches and collect
accurate per-difficulty win/loss statistics for the *simulated* (solver)
games, independent of the random broadcast stream.  Works against any host
running ms_server.py (local or deployed).

Usage:
  python ms_analyze.py --difficulty expert --games 500 --host 127.0.0.1
  python ms_analyze.py --difficulty beginner --seed 12345
  python ms_analyze.py --difficulty expert --seed 12345 --multi 50
  python ms_analyze.py --all --games 200 --out results.csv
  python ms_analyze.py --difficulty expert --seed 12345 --until-loss

The `--all` mode runs every difficulty as a batch.  `--seed` replays one exact
seed; add `--multi N` to run it N times (same board, varied solver tie-breaks)
so you see the seed's range of outcomes.  Every run - wins *and* losses - is
reported and written to the CSV.  `--until-loss` keeps replaying a seed until
a loss is observed (capped at `--multi N`, default 25), reporting how many
runs it took and how the solver lost.  Each requested game is counted only if
the server marks it with a `reqgame` line, so broadcast games on the same
connection never pollute the stats.

Protocol (client -> server):
  reqbatch <difficulty> <count>
  reqseed  <difficulty> <seed> [count]
  requntil <difficulty> <seed> [max]
The server answers each requested game with:
  reqgame <diff> <seed>
  seed    <diff> <seed>
  outcome <diff> <seed> <won> <moves> <time_ms> <guesses>
For requntil it adds lossfound / noloss, and closes every request with:
  reqdone <diff> <count>
"""

import argparse
import csv
import hashlib
import hmac
import socket
import sys
import time

DIFFS = ["beginner", "intermediate", "expert"]

GAME_FIELDS = ["difficulty", "seed", "won", "moves", "time_ms", "guesses"]


class Analyzer:
    def __init__(self, host, port, timeout=60.0):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.buf = b""
        self.loss_info = None   # dict from lossfound / noloss lines

    def _read_line(self):
        while b"\n" not in self.buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("server closed the connection")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode("ascii", "replace").strip()

    def auth(self, user, password):
        """Solver-auth challenge-response handshake (HMAC-SHA256 over a
        server nonce).  Returns True on `authok`."""
        if not user:
            return False
        self.sock.sendall(("auth %s\n" % user).encode("ascii"))
        deadline = time.time() + 10.0
        while time.time() < deadline:
            parts = self._read_line().split()
            if not parts:
                continue
            if parts[0] == "authchal":
                digest = hmac.new(
                    password.encode("utf-8"),
                    ("ms-auth:" + parts[1]).encode("ascii"),
                    hashlib.sha256).hexdigest()
                self.sock.sendall(("authresp %s\n" % digest).encode("ascii"))
            elif parts[0] == "authok":
                return True
            elif parts[0] == "autherr":
                return False
            elif parts[0] == "reqdenied":
                return False
        raise RuntimeError("timed out during solver authentication")

    def request(self, line, expected_count):
        """Send one request, drain the stream, return the finished games.

        Only games opened by a `reqgame` marker are returned.  Blocks until
        the matching `reqdone` arrives or the timeout expires.  Any
        `lossfound`/`noloss` marker is recorded in self.loss_info."""
        self.sock.sendall(line.encode("ascii") + b"\n")
        games = []
        self.loss_info = None
        pending = None          # (diff, seed) of the current requested game
        deadline = time.time() + 60.0
        while time.time() < deadline:
            parts = self._read_line().split()
            if not parts:
                continue
            if parts[0] == "reqgame":
                pending = (parts[1], int(parts[2]))
            elif parts[0] == "seed":
                pass            # re-roll marker; tracked via reqgame instead
            elif parts[0] == "outcome" and pending is not None:
                if (parts[1], int(parts[2])) == pending:
                    games.append({
                        "difficulty": parts[1],
                        "seed": int(parts[2]),
                        "won": int(parts[3]),
                        "moves": int(parts[4]),
                        "time_ms": int(parts[5]),
                        "guesses": int(parts[6]),
                    })
                    pending = None
            elif parts[0] == "lossfound":
                self.loss_info = {
                    "kind": "loss", "run": int(parts[3]), "won": int(parts[4]),
                    "moves": int(parts[5]), "time_ms": int(parts[6]),
                    "guesses": int(parts[7]),
                }
            elif parts[0] == "noloss":
                self.loss_info = {"kind": "noloss", "max": int(parts[3])}
            elif parts[0] == "reqdenied":
                raise RuntimeError(
                    "server denied the request (solver disabled or needs "
                    "credentials; pass --solver-user/--solver-pass)")
            elif parts[0] == "reqdone":
                got = int(parts[2])
                if not line.startswith("requntil") and got != expected_count:
                    print("  warning: server played %d of %d requested games"
                          % (got, expected_count), file=sys.stderr)
                return games
        raise RuntimeError("timed out waiting for reqdone (%s)" % line)

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def summarize(games, label, detail=False):
    n = len(games)
    if n == 0:
        print("  %-12s no games returned" % label)
        return None
    wins = sum(1 for g in games if g["won"])
    losses = n - wins
    mv = [g["moves"] for g in games]
    gs = [g["guesses"] for g in games]
    tm = [g["time_ms"] for g in games]
    wm = [g["moves"] for g in games if g["won"]]
    lm = [g["moves"] for g in games if not g["won"]]
    print("  %-12s games=%-5d wins=%-5d losses=%-5d win_rate=%6.2f%%"
          % (label, n, wins, losses, wins / n * 100.0))
    if wins and losses:
        print("  %-12s wins : avg_moves=%6.1f avg_guesses=%5.2f avg_time_ms=%6.1f"
              % ("", sum(wm) / wins, sum(g["guesses"] for g in games if g["won"]) / wins,
                 sum(g["time_ms"] for g in games if g["won"]) / wins))
        print("  %-12s losses: avg_moves=%6.1f avg_guesses=%5.2f avg_time_ms=%6.1f"
              % ("", sum(lm) / losses, sum(g["guesses"] for g in games if not g["won"]) / losses,
                 sum(g["time_ms"] for g in games if not g["won"]) / losses))
    print("  %-12s all   : avg_moves=%6.1f avg_guesses=%5.2f avg_time_ms=%6.1f"
          % ("", sum(mv) / n, sum(gs) / n, sum(tm) / n))
    if detail:
        print("  %-12s per-run (seed, won, moves, guesses, time_ms):"
              % "")
        for g in games:
            print("  %-12s   %20d %s %5d %5d %6d"
                  % ("", g["seed"], "win " if g["won"] else "LOSS",
                     g["moves"], g["guesses"], g["time_ms"]))
    return {"difficulty": label, "games": n, "wins": wins,
            "losses": losses, "win_rate": wins / n * 100.0,
            "avg_moves": sum(mv) / n, "avg_guesses": sum(gs) / n,
            "avg_time_ms": sum(tm) / n}


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=28571)
    ap.add_argument("--difficulty", default=None,
                    help="beginner|intermediate|expert (default: all)")
    ap.add_argument("--all", action="store_true",
                    help="run every difficulty (overrides --difficulty)")
    ap.add_argument("--games", type=int, default=200,
                    help="games per difficulty for a batch request")
    ap.add_argument("--seed", type=int, default=None,
                    help="replay one exact seed instead of a random batch")
    ap.add_argument("--multi", type=int, default=0,
                    help="with --seed: run the seed this many times "
                         "(also caps --until-loss)")
    ap.add_argument("--until-loss", action="store_true",
                    help="with --seed: replay until a loss is seen (cap "
                         "--multi N, default 25); report the losing run")
    ap.add_argument("--out", default=None,
                    help="optional CSV output path")
    ap.add_argument("--solver-user", default=None,
                    help="username for the protected solver")
    ap.add_argument("--solver-pass", default=None,
                    help="password for the protected solver (answered to an "
                         "HMAC-SHA256 challenge; never sent in the clear)")
    args = ap.parse_args()

    diffs = DIFFS if args.all else ([args.difficulty] if args.difficulty
                                    else DIFFS)
    for d in diffs:
        if d not in DIFFS:
            print("unknown difficulty %r (use %s)" % (d, "|".join(DIFFS)))
            return 2

    if args.until_loss and args.seed is None:
        print("--until-loss requires --seed")
        return 2

    print("connecting to %s:%d" % (args.host, args.port))
    a = Analyzer(args.host, args.port)
    if args.solver_user:
        print("authenticating to the solver...")
        if not a.auth(args.solver_user, args.solver_pass or ""):
            print("solver authentication FAILED", file=sys.stderr)
            a.close()
            return 3
    all_games = []
    try:
        for diff in diffs:
            if args.seed is not None and args.until_loss:
                max_runs = args.multi if args.multi > 0 else 25
                req = "requntil %s %d %d" % (diff, args.seed, max_runs)
                games = a.request(req, max_runs)
                all_games.extend(games)
                summarize(games, diff)
                li = a.loss_info
                if li and li["kind"] == "loss":
                    print("  %-12s LOSS on run %d: moves=%s guesses=%s "
                          "time_ms=%s" % ("", li["run"], li["moves"],
                                          li["guesses"], li["time_ms"]))
                else:
                    print("  %-12s no loss in %d replays (seed is strong)"
                          % ("", max_runs))
            elif args.seed is not None:
                count = args.multi if args.multi > 0 else 1
                req = "reqseed %s %d %d" % (diff, args.seed, count)
                games = a.request(req, count)
                all_games.extend(games)
                summarize(games, diff, detail=1 < count <= 25)
            else:
                req = "reqbatch %s %d" % (diff, args.games)
                games = a.request(req, args.games)
                all_games.extend(games)
                summarize(games, diff)
        print("total: %d requested games" % len(all_games))
    finally:
        a.close()

    if args.out:
        with open(args.out, "w", newline="") as fh:
            w = csv.DictWriter(fh, fieldnames=GAME_FIELDS)
            w.writeheader()
            for g in all_games:
                w.writerow(g)
        print("wrote %d rows to %s" % (len(all_games), args.out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
