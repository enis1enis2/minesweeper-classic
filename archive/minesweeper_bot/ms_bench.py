"""Benchmark harness: plays many games per difficulty/strategy.

Usage:
  python ms_bench.py --difficulty beginner --games 100 --port 31350
  python ms_bench.py --all --games 200 --port 31350
  python ms_bench.py --sweep            # sweep strategy knobs, print table

Requires a running game instance:  minesweeper-x64.exe --listen <port>
Start it from PowerShell:
  $p = Start-Process .../minesweeper-x64.exe -ArgumentList "--listen 31350" -PassThru
"""

import argparse
import random
import sys
import time

sys.path.insert(0, __import__("os").path.dirname(__file__))
from ms_client import MSClient
from ms_solver import play_game


def benchmark(port, difficulty, games, strategy, seed_base=0, verbose=False):
    strategy = dict(strategy)
    strategy.setdefault("refresh", False)  # bots don't need repaints
    client = MSClient(port)
    results = []
    start = time.time()
    for g in range(games):
        # deterministic per-game seed so different strategies see the same
        # boards (fair comparison)
        rng2 = random.Random(seed_base * 1000000 + g)
        seed = rng2.randrange(0, 2 ** 31)
        try:
            client.seed(seed)
        except Exception:
            pass
        rng = random.Random(seed)  # decision RNG derived from the board seed
        g0 = time.perf_counter()
        res = play_game(client, difficulty, strategy, rng)
        res["wall"] = time.perf_counter() - g0
        res["game"] = g
        results.append(res)
        if verbose and (g + 1) % 25 == 0:
            print(f"  {difficulty}: {g+1}/{games} games", file=sys.stderr)
    elapsed = time.time() - start
    client.close()

    wins = [r for r in results if r["win"]]
    win_rate = len(wins) / max(1, len(results)) * 100.0
    times = [r["wall"] for r in wins]
    return {
        "difficulty": difficulty,
        "games": games,
        "wins": len(wins),
        "win_rate": win_rate,
        "avg_time": (sum(times) / len(times)) if times else None,
        "fastest": min(times) if times else None,
        "slowest": max(times) if times else None,
        "avg_moves": sum(r["moves"] for r in wins) / len(wins) if wins else None,
        "avg_chords": sum(r["chords"] for r in wins) / len(wins) if wins else None,
        "strategy": strategy,
        "wall_s": elapsed,
    }


STRATEGIES = [
    {"name": "minprob-random", "tiebreak": "random"},
    {"name": "minprob-info", "tiebreak": "info"},
]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--difficulty", default="beginner")
    ap.add_argument("--games", type=int, default=100)
    ap.add_argument("--port", type=int, default=31350)
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--sweep", action="store_true")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()

    diffs = ["beginner", "intermediate", "expert"] if args.all else [args.difficulty]
    seed_base = {"beginner": 0, "intermediate": 1, "expert": 2}

    if args.sweep:
        variants = []
        for tie in ("random", "info"):
            for first in ("center", "corner"):
                for chord in (True, False):
                    variants.append({"name": f"tie={tie},first={first},chord={chord}",
                                     "tiebreak": tie, "first": first,
                                     "use_chord": chord})
    else:
        variants = STRATEGIES

    for d in diffs:
        print(f"\n===== {d} =====")
        rows = []
        for v in variants:
            strat = {k: val for k, val in v.items() if k != "name"}
            r = benchmark(args.port, d, args.games, strat,
                          seed_base=seed_base[d], verbose=args.verbose)
            r["name"] = v["name"]
            rows.append(r)
            wr = r["win_rate"]
            ft = f"{r['fastest']:.2f}" if r["fastest"] is not None else "-"
            at = f"{r['avg_time']:.2f}" if r["avg_time"] is not None else "-"
            print(f"  {v['name']:<32} win={wr:6.2f}%  "
                  f"fastest={ft:>6}s  avg={at:>6}s  "
                  f"avg_moves={r['avg_moves'] if r['avg_moves'] else 0:6.1f}")
        best = max(rows, key=lambda r: (r["win_rate"], -(r["avg_time"] or 0)))
        print(f"  -> best: {best['name']} win={best['win_rate']:.2f}%")


if __name__ == "__main__":
    main()
