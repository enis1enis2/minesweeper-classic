"""Quick smoke test: play N games and print results."""
import argparse
import random
import sys
import os

sys.path.insert(0, os.path.dirname(__file__))
from ms_client import MSClient
from ms_solver import play_game


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=31350)
    ap.add_argument("--games", type=int, default=10)
    ap.add_argument("--difficulty", default="beginner")
    args = ap.parse_args()

    c = MSClient(args.port)
    rng = random.Random(1)
    wins = 0
    for g in range(args.games):
        c.seed(1000 + g)
        res = play_game(c, args.difficulty, {"tiebreak": "info"}, rng)
        wins += res["win"]
        status = "WIN " if res["win"] else "LOSS"
        print(f"{status} time={res['time']}s moves={res['moves']} "
              f"chords={res['chords']} flags={res['flags']} guesses={res['guesses']}")
    print(f"\n{args.difficulty}: {wins}/{args.games} won ({wins/args.games*100:.1f}%)")
    c.close()


if __name__ == "__main__":
    main()
