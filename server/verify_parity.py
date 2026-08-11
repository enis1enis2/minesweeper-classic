"""verify_parity.py - prove the simulated board matches the real game.

For a set of (difficulty, seed, first-click) triples this tool:
  1. starts minesweeper-x64.exe --listen <port>,
  2. applies the seed as a persistent Normal seed and plays the first click
     through the scripting CLI,
  3. builds the same board in sim_engine and clicks the same cell,
  4. compares the two `board` dumps cell-by-cell (and `opened`/`over`).

A mismatch means the sim's xorshift64 / place_mines / reveal logic drifted
from the C code.  Run from the server/ directory (Windows only; needs the
compiled exe).

Usage:
  python verify_parity.py [--exe ..\\build\\minesweeper-x64.exe]
      [--port 31350] [--seeds 8] [--fast]
"""

import argparse
import os
import random
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

from sim_engine import SimBoard  # noqa: E402

DIFFS = ["beginner", "intermediate", "expert"]
# (rows, cols) per difficulty, used to pick first-click positions.
SIZES = {"beginner": (8, 8), "intermediate": (16, 16), "expert": (16, 30)}
FIRST_CLICKS = ["center", "corner", "edge"]


def first_cell(diff, pos):
    rows, cols = SIZES[diff]
    if pos == "center":
        return rows // 2, cols // 2
    if pos == "corner":
        return 0, 0
    return rows // 2, 0


def find_free_port(base):
    for p in range(base, base + 200):
        s = socket.socket()
        try:
            s.bind(("127.0.0.1", p))
            s.close()
            return p
        except OSError:
            s.close()
    raise RuntimeError("no free port near %d" % base)


def wait_port(port, exe_proc, timeout=15):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if exe_proc.poll() is not None:
            raise RuntimeError("exe exited early with code %r" %
                               exe_proc.returncode)
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=1)
            s.close()
            return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError("game did not open port %d" % port)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", default=os.path.join(ROOT, "build",
                                                  "minesweeper-x64.exe"))
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--seeds", type=int, default=8)
    ap.add_argument("--fast", action="store_true",
                    help="skip expert/edge combos for a quicker pass")
    args = ap.parse_args()

    sys.path.insert(0, os.path.join(ROOT, "minesweeper_bot"))
    from ms_client import MSClient

    if not os.path.exists(args.exe):
        print("exe not found: %s (build it first)" % args.exe)
        return 2
    port = args.port or find_free_port(31350)

    exe = subprocess.Popen([args.exe, "--listen", str(port)],
                           stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
    try:
        wait_port(port, exe)
        client = MSClient(port)
        if not client.ping():
            print("game did not answer ping")
            return 2

        rng = random.Random(20260209)
        fails = 0
        total = 0
        combos = []
        for diff in DIFFS:
            positions = FIRST_CLICKS[:]
            if args.fast and diff == "expert":
                positions = ["center"]
            for pos in positions:
                for _ in range(args.seeds):
                    combos.append((diff, pos, rng.randrange(0, 1 << 63)))

        for diff, pos, seed in combos:
            total += 1
            r0, c0 = first_cell(diff, pos)
            client.seed_diff(diff, seed)
            client.new(diff)
            client.click(r0, c0)
            live_state = client.state()
            live_board = client.board()

            sim = SimBoard()
            sim.new(diff, seed)
            sim.click(r0, c0)
            sim_board = sim.board()
            sim_state = sim.state()

            mismatches = []
            if sim_board != live_board:
                mismatches.append("board differs")
            for key in ("opened", "over", "started"):
                if sim_state.get(key) != live_state.get(key):
                    mismatches.append("%s %s!=%s" % (
                        key, sim_state.get(key), live_state.get(key)))
            if mismatches:
                fails += 1
                print("  MISMATCH %s seed=%d first=(%d,%d) pos=%s: %s"
                      % (diff, seed, r0, c0, pos, "; ".join(mismatches)))
                for i, (a, b) in enumerate(zip(live_board, sim_board)):
                    if a != b:
                        print("    live row %2d: %s" % (i, a))
                        print("    sim  row %2d: %s" % (i, b))
                        break

        print("parity: %d/%d boards identical (%d mismatch%s)"
              % (total - fails, total, fails, "es" if fails != 1 else ""))
        client.close()
        return 0 if fails == 0 else 1
    finally:
        try:
            exe.terminate()
        except OSError:
            pass


if __name__ == "__main__":
    sys.exit(main())
