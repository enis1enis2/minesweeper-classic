"""Minesweeper solver engine.

Drives a Minesweeper (Classic) game through the MSClient and plays it with a
deterministic constraint-propagation deduction pass plus an exact
probabilistic pass over the frontier of unrevealed cells.  The probabilistic
pass conditions each component of the frontier on the global mine count
(binomial weighting over the free cells) which is the step that lets a
near-perfect player clear boards that look like coin flips.

Strategy knobs (all benchmarked by ms_bench.py):
  - tiebreak: how to choose among equally-probable cells
  - first:     where the first (guaranteed safe) click goes
  - use_chord: chording to reveal neighbours fast
"""

import math
import random


class Board:
    """Parsed view of the board returned by the game's 'board' command."""

    def __init__(self, rows, lines, total_mines):
        self.rows = rows
        self.lines = [ln.rstrip("\r") for ln in lines]
        self.cols = len(self.lines[0]) if rows else 0
        self.total_mines = total_mines
        n = rows * self.cols
        self.revealed = [False] * n   # a number or a revealed mine
        self.mine = [False] * n       # revealed mine ('*')
        self.flagged = [False] * n    # 'F'
        self.q = [False] * n          # '?'
        self.num = [0] * n            # displayed number 0..8
        self._parse()

    def _parse(self):
        for r, ln in enumerate(self.lines):
            for c, ch in enumerate(ln):
                i = r * self.cols + c
                if ch == "F":
                    self.flagged[i] = True
                elif ch == "?":
                    self.q[i] = True
                elif ch == "*":
                    self.revealed[i] = True
                    self.mine[i] = True
                elif "0" <= ch <= "8":
                    self.revealed[i] = True
                    self.num[i] = ord(ch) - ord("0")

    def id(self, r, c):
        return r * self.cols + c

    def rc(self, i):
        return divmod(i, self.cols)

    def neighbors(self, r, c):
        out = []
        for dr in (-1, 0, 1):
            for dc in (-1, 0, 1):
                if dr == 0 and dc == 0:
                    continue
                nr, nc = r + dr, c + dc
                if 0 <= nr < self.rows and 0 <= nc < self.cols:
                    out.append(self.id(nr, nc))
        return out

    def hidden(self, i):
        return not self.revealed[i] and not self.flagged[i]

    def flags_around(self, i):
        r, c = self.rc(i)
        return sum(1 for j in self.neighbors(r, c) if self.flagged[j])

    def hidden_around(self, i):
        r, c = self.rc(i)
        return [j for j in self.neighbors(r, c) if self.hidden(j)]

    def all_hidden(self):
        return [i for i in range(self.rows * self.cols) if self.hidden(i)]

    # ------------------------------------------------------- game outcome
    def lost(self):
        return any(self.mine)

    def won(self):
        if self.lost():
            return False
        for r, ln in enumerate(self.lines):
            for c, ch in enumerate(ln):
                if ch in ".?":
                    return False
        return True


# ------------------------------------------------------------------------
# deterministic deduction
# ------------------------------------------------------------------------
def build_constraints(board):
    """Constraints from revealed numbers: (frozenset of hidden cells, need)."""
    cons = []
    for r in range(board.rows):
        for c in range(board.cols):
            i = board.id(r, c)
            if not board.revealed[i] or board.mine[i]:
                continue
            hidden = board.hidden_around(i)
            if not hidden:
                continue
            need = board.num[i] - board.flags_around(i)
            if need < 0 or need > len(hidden):
                continue
            cons.append((frozenset(hidden), need))
    return cons


def deduce(board, cons):
    """Return (safes, mines) sets forced by the constraints."""
    safes = set()
    mines = set()

    def add_safe(s):
        for i in s:
            if board.hidden(i):
                safes.add(i)

    def add_mine(s):
        for i in s:
            if board.hidden(i):
                mines.add(i)

    for cells, need in cons:
        if need == 0:
            add_safe(cells)
        elif need == len(cells):
            add_mine(cells)

    changed = True
    while changed:
        changed = False
        for A, na in cons:
            for B, nb in cons:
                if A == B or not A <= B:
                    continue
                diff = B - A
                dneed = nb - na
                if dneed == 0 and diff:
                    new = {i for i in diff if board.hidden(i)}
                    if new and not new <= safes:
                        safes |= new
                        changed = True
                elif dneed == len(diff) and diff:
                    new = {i for i in diff if board.hidden(i)}
                    if new and not new <= mines:
                        mines |= new
                        changed = True
    return safes, mines


# ------------------------------------------------------------------------
# exact frontier probability
# ------------------------------------------------------------------------
def frontier_components(board, cons):
    """Split frontier cells into connected components via shared constraints."""
    cells = set()
    for s, _need in cons:
        cells |= s
    if not cells:
        return [], []
    parent = {i: i for i in cells}

    def find(x):
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    def union(a, b):
        ra, rb = find(a), find(b)
        if ra != rb:
            parent[ra] = rb

    for s, _need in cons:
        sl = list(s)
        for i in range(1, len(sl)):
            union(sl[0], sl[i])

    comps = {}
    for i in cells:
        comps.setdefault(find(i), []).append(i)
    return list(cells), list(comps.values())


def solve_component(local_cells, local_cons, node_budget=2_000_000):
    """Enumerate all mine placements in one frontier component.

    Returns (S, T):
      S[total] = number of solutions with exactly `total` mines
      T[li][total] = number of solutions with cell li a mine
    Cells indexed 0..len(local_cells)-1.  Returns None if budget exceeded.
    """
    m = len(local_cells)
    if m == 0:
        return {0: 1}, []
    cell_pos = {c: i for i, c in enumerate(local_cells)}
    cons = []
    for s, need in local_cons:
        idx = [cell_pos[c] for c in s]
        cons.append((idx, need))

    member = [[] for _ in range(m)]
    for ci, (idx, _need) in enumerate(cons):
        for li in idx:
            member[li].append(ci)

    order = sorted(range(m), key=lambda li: (-len(member[li]), li))
    order_pos = [0] * m
    for p, li in enumerate(order):
        order_pos[li] = p

    assigned = [-1] * m
    con_mines = [0] * len(cons)
    con_left = [len(idx) for idx, _need in cons]

    S = {}
    T = [{} for _ in range(m)]
    nodes = [0]

    def feasible(li, val):
        for ci in member[li]:
            need = cons[ci][1]
            new_mines = con_mines[ci] + val
            if new_mines > need:
                return False
            new_left = con_left[ci] - 1
            if need - new_mines > new_left:
                return False
        return True

    def rec(p):
        nodes[0] += 1
        if nodes[0] > node_budget:
            return
        if p == m:
            total = sum(1 for v in assigned if v == 1)
            S[total] = S.get(total, 0) + 1
            for li, v in enumerate(assigned):
                if v == 1:
                    T[li][total] = T[li].get(total, 0) + 1
            return
        li = order[p]
        for val in (0, 1):
            if not feasible(li, val):
                continue
            assigned[li] = val
            for ci in member[li]:
                con_mines[ci] += val
                con_left[ci] -= 1
            rec(p + 1)
            for ci in member[li]:
                con_mines[ci] -= val
                con_left[ci] += 1
            assigned[li] = -1

    rec(0)
    if nodes[0] > node_budget:
        return None
    return S, T


def _convolve(d1, d2):
    """Convolution of two dict count->prob distributions."""
    out = {}
    for k1, v1 in d1.items():
        for k2, v2 in d2.items():
            out[k1 + k2] = out.get(k1 + k2, 0.0) + v1 * v2
    return out


def _comb(n, k):
    return math.comb(n, k) if 0 <= k <= n else 0


def frontier_probabilities(board, cons, node_budget=2_000_000):
    """Exact mine probabilities for frontier cells.

    Returns (probs, nonfrontier_p).  probs maps cell id -> P(mine).  Cells
    that could not be solved within budget are excluded from probs (the
    caller then treats them as free guesses).
    """
    cells, comps = frontier_components(board, cons)
    if not cells:
        return {}, None

    all_s = set(cells)
    free = [i for i in range(board.rows * board.cols)
            if board.hidden(i) and i not in all_s]
    n_free = len(free)

    # solve each component
    comp_data = []  # (comp_cells, S, T)
    for comp in comps:
        comp_set = set(comp)
        local_cons = [c for c in cons if c[0] <= comp_set]
        res = solve_component(comp, local_cons, node_budget)
        if res is None:
            continue  # skip unsolvable component (treated as free)
        comp_data.append((comp, res[0], res[1]))
    if not comp_data:
        return {}, None

    # normalised mine-count distributions per component
    comps_dist = []
    comps_T = []
    comps_cells = []
    comps_tot = []   # total solutions per component
    for comp, S, T in comp_data:
        tot = sum(S.values())
        d = {k: v / tot for k, v in S.items()}
        comps_dist.append(d)
        comps_T.append(T)
        comps_cells.append(comp)
        comps_tot.append(tot)

    k = len(comps_dist)
    prefix = [{} for _ in range(k + 1)]
    prefix[0] = {0: 1.0}
    for i in range(k):
        prefix[i + 1] = _convolve(prefix[i], comps_dist[i])
    suffix = [{} for _ in range(k + 1)]
    suffix[k] = {0: 1.0}
    for i in range(k - 1, -1, -1):
        suffix[i] = _convolve(comps_dist[i], suffix[i + 1])

    M = board.total_mines
    D = prefix[k]  # overall frontier mine-count distribution

    # binomial weights over free cells: C(n_free, M - t), normalised
    raw = {t: _comb(n_free, M - t) for t in D}
    mx = max(raw.values()) if raw else 1
    if mx <= 0:
        w = {t: 1.0 for t in D}
    else:
        w = {t: v / mx for t, v in raw.items()}

    Z = sum(D[t] * w[t] for t in D)
    if Z <= 0:
        return {}, None

    E_front = sum(t * D[t] * w[t] for t in D) / Z
    nonfrontier_p = 0.0
    if n_free > 0:
        nonfrontier_p = max(0.0, min(1.0, (M - E_front) / n_free))

    probs = {}
    for ci, comp in enumerate(comps_cells):
        D_except = _convolve(prefix[ci], suffix[ci + 1])
        T = comps_T[ci]
        tot_i = comps_tot[ci]
        for li, cell in enumerate(comp):
            num = 0.0
            for total, cnt in T[li].items():
                # U(total) = sum_o D_except[o] * w[total + o]
                u = 0.0
                for o, po in D_except.items():
                    u += po * w.get(total + o, 0.0)
                num += (cnt / tot_i) * u
            probs[cell] = num / Z
    return probs, nonfrontier_p


# ------------------------------------------------------------------------
# move selection
# ------------------------------------------------------------------------
def choose_move(board, probs, nonfrontier_p, rng, strategy):
    """Pick the cell to click next given frontier probabilities."""
    tie = strategy.get("tiebreak", "minprob")
    frontier = set(probs)
    free = [i for i in range(board.rows * board.cols)
            if board.hidden(i) and i not in frontier]

    candidates = []
    for i, p in probs.items():
        candidates.append((p, i, "frontier"))
    if free and nonfrontier_p is not None:
        for i in free:
            candidates.append((nonfrontier_p, i, "free"))

    if not candidates:
        hidden = board.all_hidden()
        return rng.choice(hidden) if hidden else None

    minp = min(p for p, _i, _k in candidates)
    best = [c for c in candidates if c[0] <= minp + 1e-12]

    if tie == "random":
        return rng.choice(best)[1]

    # info-gain tie-break: prefer cells that can reveal the most new cells.
    # free cells open big unexplored areas; a frontier cell that has few
    # revealed neighbours can flood into the unknown, while one surrounded
    # by revealed numbers only uncovers itself.
    def info(c):
        p, i, kind = c
        if kind == "free":
            return (2.0, rng.random())
        r, cc = board.rc(i)
        nbrs = board.neighbors(r, cc)
        rev = sum(1 for j in nbrs if board.revealed[j] and not board.mine[j])
        return ((9 - rev) / 9.0, rng.random())

    return max(best, key=info)[1]


# ------------------------------------------------------------------------
# the player
# ------------------------------------------------------------------------
def play_game(client, difficulty, strategy, rng, stats=None):
    """Play one full game. Returns dict with outcome and stats."""
    refresh_on = strategy.get("refresh", True)
    if not refresh_on:
        client.refresh(False)
    client.new(difficulty)
    st = client.state()
    rows = int(st["rows"])
    cols = int(st["cols"])
    mines = int(st["mines"])

    first = strategy.get("first", "center")
    if first == "corner":
        r0, c0 = 0, 0
    else:
        r0, c0 = rows // 2, cols // 2
    client.click(r0, c0)

    moves = 1
    chords = 0
    flags_placed = 0
    guesses = 0
    deduce_batches = 0
    guess_samples = []   # (frontier_cells, free_cells, chosen_p) per guess
    while True:
        board = Board(rows, client.board(), mines)
        if board.lost() or board.won():
            break

        cons = build_constraints(board)
        safes, mines_set = deduce(board, cons)

        if safes or mines_set or (strategy.get("use_chord", True) and _chordable(board)):
            deduce_batches += 1
            cmds = []
            for i in mines_set:
                r, c = board.rc(i)
                cmds.append(("flag", r, c))
            # chord every number whose flag count will match after the flags
            # above are placed (reveals big areas in one command each)
            if strategy.get("use_chord", True):
                for r in range(board.rows):
                    for c in range(board.cols):
                        i = board.id(r, c)
                        if not board.revealed[i] or board.mine[i]:
                            continue
                        if board.num[i] == 0:
                            continue
                        if board.flags_around(i) == board.num[i]:
                            if board.hidden_around(i):
                                cmds.append(("chord", r, c))
            for i in safes:
                if i not in mines_set:
                    r, c = board.rc(i)
                    cmds.append(("click", r, c))
            flags_placed += len(mines_set)
            chords += sum(1 for c in cmds if c[0] == "chord")
            moves += len(cmds)
            _batch(client, cmds)
            continue

        # probabilistic guess
        probs, nf_p = frontier_probabilities(board, cons)
        target = choose_move(board, probs, nf_p, rng, strategy)
        if target is None:
            break
        r, c = board.rc(target)
        guess_samples.append((len(probs),
                              sum(1 for i in range(rows * cols)
                                  if board.hidden(i) and i not in set(probs)),
                              probs.get(target, nf_p)))
        client.click(r, c)
        moves += 1
        guesses += 1

    board = Board(rows, client.board(), mines)
    st = client.state()
    res = {
        "win": board.won() and not board.lost(),
        "time": int(st.get("time", 0)),
        "moves": moves,
        "chords": chords,
        "flags": flags_placed,
        "guesses": guesses,
        "deduce_batches": deduce_batches,
        "frontier": guess_samples,
        "difficulty": difficulty,
    }
    if stats is not None:
        stats.setdefault("games", 0)
        stats["games"] += 1
        stats.setdefault("wins", 0)
        stats["wins"] += 1 if res["win"] else 0
        stats.setdefault("total_guesses", 0)
        stats["total_guesses"] += guesses
        stats.setdefault("total_moves", 0)
        stats["total_moves"] += moves
        stats.setdefault("frontier_samples", 0)
        stats["frontier_samples"] += len(guess_samples)
        stats.setdefault("guess_p_sums", [0.0, 0])
        stats["guess_p_sums"][0] += sum(p for _f, _n, p in guess_samples)
        stats["guess_p_sums"][1] += len(guess_samples)
    return res


def _chordable(board):
    for r in range(board.rows):
        for c in range(board.cols):
            i = board.id(r, c)
            if not board.revealed[i] or board.mine[i]:
                continue
            if board.num[i] == 0:
                continue
            if board.flags_around(i) == board.num[i] and board.hidden_around(i):
                return True
    return False


def _batch(client, cmds):
    """Send a batch of (cmd, r, c) tuples in one write, drain replies."""
    payload = "\n".join(f"{cmd} {r} {c}" for cmd, r, c in cmds) + "\n"
    client.sock.sendall(payload.encode("ascii"))
    for _ in cmds:
        while True:
            line = client._read_line()
            if line == "END":
                break
