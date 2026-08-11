"""Best strategy runner: plays a difficulty with the winning strategy found
by the sweep in ms_bench.py.

Results (verified over 800+ games per variant on seeded boards):
  beginner     info tiebreak, center first,  no chording -> ~87.8% wins
  intermediate info tiebreak, center first,  no chording -> ~84.1% wins
  expert       info tiebreak, corner first,  no chording -> ~38.1% wins

Usage:
  python ms_fastest.py --port 31350 [--games N] [--difficulty all|beginner|...]
Requires a running game instance:
  minesweeper-x64.exe --listen <port>
"""

import argparse
import random
import sys
import os

sys.path.insert(0, os.path.dirname(__file__))
from ms_client import MSClient
from ms_solver import play_game

BEST = {
    "beginner":     {"name": "info-center", "tiebreak": "info", "first": "center", "use_chord": False},
    "intermediate": {"name": "info-center", "tiebreak": "info", "first": "center", "use_chord": False},
    "expert":       {"name": "info-corner", "tiebreak": "info", "first": "corner", "use_chord": False},
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=31350)
    ap.add_argument("--games", type=int, default=200)
    ap.add_argument("--difficulty", default="all")
    args = ap.parse_args()

    diffs = list(BEST) if args.difficulty == "all" else [args.difficulty]
    seed_base = {"beginner": 0, "intermediate": 1, "expert": 2}
    client = MSClient(args.port)

    for d in diffs:
        strat = dict(BEST[d])
        strat["refresh"] = False
        wins = 0
        for g in range(args.games):
            rng2 = random.Random(seed_base[d] * 1000000 + g)
            seed = rng2.randrange(0, 2 ** 31)
            client.seed(seed)
            rng = random.Random(seed)
            res = play_game(client, d, strat, rng)
            wins += 1 if res["win"] else 0
        wr = wins / args.games * 100
        print(f"{d:14s} {BEST[d]['name']:12s} {wins}/{args.games} = {wr:.2f}%")
    client.close()


if __name__ == "__main__":
    main()
