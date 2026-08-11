"""py_ref.py - Python reference driver for the Node <-> Python differential.

Run by ms/tools/parity-all.js (and indirectly by ms/test/parity-python.test.js)
when `python` is on PATH.  Reads one JSON request per line on stdin and writes
one JSON reply per line on stdout, so the Node harness stays the single source
of truth for inputs and the two runtimes never share formatting assumptions.

Modes (selected by the first command-line argument):

  py_ref.py sim
      Each request: {op: "sim", scripts: [[ops...], ...]}
      ops are {"o": "new", "d": diff, "s": seed} | {"o": "click|flag|chord",
      "r": r, "c": c} | {"o": "board"} | {"o": "state"}
      Reply: {board: [lines...], state: {k: v, ...}} per script.

  py_ref.py solve
      Each request: {op: "solve", cases: [{id, rng_seed, rows, cols}, ...]}
      The Python side generates each mid-game board with the same gen_board()
      as tools/analyze_diff.py (seeded by rng_seed) and returns the board text
      PLUS its own solver results, so the Node harness can solve the exact
      same board and compare independently.
      Reply: {id, rows, cols, mines, lines, cons: [[need, [cells...]]...],
              safe: [cells...], mine: [cells...], probs: [[cell, p]...],
              nfp: float|null}

Both modes feed the same inputs to the Python reference modules
(server/sim_engine.py and minesweeper_bot/ms_solver.py) and the Node port
mirrors them, so any drift in board generation, deduction or exact frontier
probabilities shows up as a mismatch.
"""

import json
import os
import random
import sys

_REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
for _sub in ("server", "minesweeper_bot"):
    _p = os.path.join(_REPO, _sub)
    if _p not in sys.path:
        sys.path.insert(0, _p)


def run_sim(scripts):
    from sim_engine import SimBoard

    out = []
    for script in scripts:
        b = SimBoard()
        for op in script:
            o = op["o"]
            if o == "new":
                b.new(op["d"], int(op["s"]))
            elif o == "click":
                b.click(op["r"], op["c"])
            elif o == "flag":
                b.flag(op["r"], op["c"])
            elif o == "chord":
                b.chord(op["r"], op["c"])
            elif o == "board":
                pass  # terminal op; nothing to do
        out.append({"board": b.board(), "state": b.state()})
    return out


def _sorted(obj):
    return sorted(obj)


def gen_board(rng, rows, cols):
    """Mirror of tools/analyze_diff.py gen_board (same RNG stream semantics)."""
    n = rows * cols
    mines_total = rng.randint(max(2, n // 12), n // 6)
    mines = set(rng.sample(range(n), mines_total))

    centre = rng.randrange(n)
    while centre in mines:
        centre = rng.randrange(n)

    revealed = {centre}
    frontier = [centre]
    while frontier:
        x = frontier.pop()
        if len(revealed) > n // 3:
            break
        r, c = divmod(x, cols)
        nbrs = []
        for dr in (-1, 0, 1):
            for dc in (-1, 0, 1):
                if dr == 0 and dc == 0:
                    continue
                nr, nc = r + dr, c + dc
                if 0 <= nr < rows and 0 <= nc < cols:
                    nbrs.append(nr * cols + nc)
        for y in nbrs:
            if y in mines or y in revealed:
                continue
            if rng.random() < 0.5:
                revealed.add(y)
                frontier.append(y)

    def nbr_mines(r, c):
        cnt = 0
        for dr in (-1, 0, 1):
            for dc in (-1, 0, 1):
                if dr == 0 and dc == 0:
                    continue
                nr, nc = r + dr, c + dc
                if 0 <= nr < rows and 0 <= nc < cols and nr * cols + nc in mines:
                    cnt += 1
        return cnt

    flagged = set()
    hid = [i for i in range(n) if i not in revealed and i not in mines]
    rng.shuffle(hid)
    for i in hid[: rng.randint(0, min(4, len(hid)))]:
        r, c = divmod(i, cols)
        touching = any(
            0 <= r + dr < rows and 0 <= c + dc < cols
            and (r + dr) * cols + (c + dc) in revealed
            for dr in (-1, 0, 1) for dc in (-1, 0, 1)
            if not (dr == 0 and dc == 0)
        )
        if touching:
            flagged.add(i)

    board = []
    for r in range(rows):
        line = []
        for c in range(cols):
            i = r * cols + c
            if i in mines:
                line.append(".")
            elif i in flagged:
                line.append("F")
            elif i in revealed:
                line.append(str(nbr_mines(r, c)))
            else:
                line.append(".")
        board.append("".join(line))
    return mines_total, board


def run_solve(cases):
    from ms_solver import (
        Board,
        build_constraints,
        deduce,
        frontier_probabilities,
    )

    out = []
    for case in cases:
        rng = random.Random(case["rng_seed"])
        mines_total, lines = gen_board(rng, case["rows"], case["cols"])
        b = Board(case["rows"], lines, mines_total)
        cons = build_constraints(b)
        safe, mine = deduce(b, cons)
        probs, nfp = frontier_probabilities(b, cons)
        out.append(
            {
                "id": case["id"],
                "rows": case["rows"],
                "cols": case["cols"],
                "mines": mines_total,
                "lines": lines,
                "cons": [[need, _sorted(cells)] for cells, need in cons],
                "safe": _sorted(safe),
                "mine": _sorted(mine),
                "probs": [[cell, p] for cell, p in sorted(probs.items())],
                "nfp": nfp,
            }
        )
    return out


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "sim"
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        req = json.loads(line)
        if req.get("op") == "sim":
            reply = {"reply": run_sim(req["scripts"])}
        elif req.get("op") == "solve":
            reply = {"reply": run_solve(req["cases"])}
        else:
            reply = {"error": "unknown op " + repr(req.get("op"))}
        print(json.dumps(reply), flush=True)


if __name__ == "__main__":
    main()
