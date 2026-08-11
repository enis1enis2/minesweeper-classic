"""analyze_diff.py - differential test: C engine vs Python solver.

Generates random mid-game boards, runs both the C harness (analyze_test.exe)
and the Python reference (analyze_test.py) on each, and reports any cell
whose P(mine) differs by more than 1e-9 or whose reveal-count differs.
"""
import random
import subprocess
import sys

C_TOOL = r"build\analyze_test.exe"


def gen_board(rng, rows, cols):
    """Return (mines_total, board_rows) for a random mid-game state."""
    n = rows * cols
    mines_total = rng.randint(max(2, n // 12), n // 6)
    mines = set(rng.sample(range(n), mines_total))

    # reveal a random blob of non-mine cells around a centre that is not a mine
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
    # flag a few hidden cells next to numbers to exercise need != adj
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
                line.append(".")          # mine stays hidden during play
            elif i in flagged:
                line.append("F")
            elif i in revealed:
                line.append(str(nbr_mines(r, c)))
            else:
                line.append(".")
        board.append("".join(line))
    return mines_total, board


def run_c(board_text):
    p = subprocess.run([C_TOOL], input=board_text, capture_output=True,
                       text=True, encoding="ascii")
    return p.returncode, p.stdout


def run_py(board_text):
    p = subprocess.run([sys.executable, "tools/analyze_test.py"],
                       input=board_text, capture_output=True, text=True,
                       encoding="ascii")
    return p.returncode, p.stdout


def main():
    seed0 = int(sys.argv[1]) if len(sys.argv) > 1 else 1
    count = int(sys.argv[2]) if len(sys.argv) > 2 else 500
    rng = random.Random(seed0)
    failures = 0
    for t in range(count):
        rows = rng.randint(4, 9)
        cols = rng.randint(4, 9)
        mines, board = gen_board(rng, rows, cols)
        text = f"{rows} {cols} {mines}\n" + "\n".join(board) + "\n"

        rc_c, out_c = run_c(text)
        rc_p, out_p = run_py(text)

        if rc_c != 0 or rc_p != 0:
            print(f"=== board {t} (rc c={rc_c} py={rc_p}) ===")
            print(text)
            print("C :", out_c.strip())
            print("PY:", out_p.strip())
            failures += 1
            continue

        lines_c = [l for l in out_c.splitlines() if l and not l.startswith("#")]
        lines_p = [l for l in out_p.splitlines() if l and not l.startswith("#")]
        if len(lines_c) != len(lines_p):
            print(f"=== board {t}: line count {len(lines_c)} vs {len(lines_p)} ===")
            print(text)
            failures += 1
            continue

        for a, b in zip(lines_c, lines_p):
            ca = [float(x) for x in a.split()[:4]]
            cb = [float(x) for x in b.split()[:4]]
            ia = int(a.split()[5])
            ib = int(b.split()[5])
            if abs(ca[2] - cb[2]) > 1e-9 or abs(ca[3] - cb[3]) > 1e-9 \
                    or ia != ib:
                print(f"=== board {t} mismatch cell {a.split()[0]} ===")
                print(text)
                print("C :", a)
                print("PY:", b)
                failures += 1
    print(f"done: {count} boards, {failures} mismatches")


if __name__ == "__main__":
    main()
