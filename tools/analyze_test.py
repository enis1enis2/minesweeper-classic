"""analyze_test.py - reference probabilities via the Python solver.

Reads the same board format as analyze_test.c from stdin and prints the
identical per-cell layout so the two can be diffed.

  line 1: rows cols mines
  lines 2..: one row per line; '0'-'8' revealed number, '*' revealed mine,
             'F' flag, '?' question-mark, anything else hidden.
"""
import sys

sys.path.insert(0, "archive/minesweeper_bot")

from ms_solver import Board, build_constraints, frontier_probabilities  # noqa: E402


def main():
    lines = sys.stdin.read().splitlines()
    rows, cols, mines = (int(x) for x in lines[0].split())
    board = Board(rows, lines[1:], mines)

    cons = build_constraints(board)
    probs, nf = frontier_probabilities(board, cons)

    n = rows * cols
    hidden = [i for i in range(n) if board.hidden(i)]
    if not probs and nf is None and hidden:
        # C engine's uniform fallback: nothing is solvable, every hidden
        # cell is a fair guess (the solver would just guess at random)
        nf = mines / len(hidden)
        probs = {}
    for i in hidden:
        p = probs.get(i, nf if nf is not None else 0.0)
        if p is None:
            p = 0.0
        # reveals: flood from i over hidden un-flagged cells, stopping at numbers
        revealed_open = set()
        stack = [i]
        while stack:
            x = stack.pop()
            if x in revealed_open:
                continue
            revealed_open.add(x)
            if board.num[x] == 0:  # zero cell opens neighbours
                r, cc = board.rc(x)
                for nb in board.neighbors(r, cc):
                    if board.hidden(nb) and nb not in revealed_open:
                        stack.append(nb)
        print(f"{i} {i // cols} {i % cols} {p:.12g} {1.0 - p:.12g} "
              f"{len(revealed_open)} {int(i in probs)}")
    nf_show = nf if nf is not None else 0.0
    print(f"# {len(hidden)} {len(hidden) - len(set(probs))} {nf_show:.12g}")


if __name__ == "__main__":
    main()
