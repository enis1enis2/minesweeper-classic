"""sim_engine.py - headless port of the C game's board generation and clicks.

Implements, bit-for-bit, the board layout used by the real minesweeper.exe
so simulated games and live games agree on the same seed:

  * xorshift64 PRNG (minesweeper.c xorshift())
  * place_mines(): pool of every cell outside the first click's 3x3, then a
    partial Fisher-Yates draw using  k = rng() % n   while n shrinks; tiny
    boards fall back to "only the clicked cell is safe".
  * reveal_cell() flood fill with the auto-win / auto-flag-mines rules and
    the auto-reveal-mines rule on loss.

The board's click/flag/chord/board/state protocol matches the scripting CLI
(minesweeper.c cli_*) so ms_solver.py's play_game() drives it unmodified
through the SimClient adapter.
"""

import sys

MASK64 = (1 << 64) - 1

# A resolved seed of zero would leave xorshift64 stuck at zero forever (every
# output step is x ^= x<<13 ^ x>>7 ^ x<<17 with x == 0), so seed 0 is mapped
# onto a fixed nonzero constant.  This must stay identical in minesweeper.c
# (RNG_ZERO_SEED_FALLBACK) and ms/core/sim-engine.js.
ZERO_SEED_FALLBACK = 0x9E3779B97F4A7C15


def normalise_seed(seed):
    """Mask to uint64, mapping 0 onto ZERO_SEED_FALLBACK (see above)."""
    s = seed & MASK64
    return s if s else ZERO_SEED_FALLBACK

# (rows, cols, mines) per difficulty, mirroring g_presets[].
PRESETS = {
    "beginner": (8, 8, 10),
    "intermediate": (16, 16, 40),
    "expert": (16, 30, 99),
}

DEFAULTS = {"beginner": "beginner", "intermediate": "intermediate",
            "expert": "expert"}


class Rng64:
    """The game's xorshift64, advanced in place (one value per call)."""

    def __init__(self, seed):
        self.s = normalise_seed(seed)

    def next(self):
        x = self.s
        x ^= (x << 13) & MASK64
        x ^= x >> 7
        x ^= (x << 17) & MASK64
        x &= MASK64
        self.s = x
        return x


def xorshift(state):
    """Single-step convenience; state is a 1-element list (mutable holder)."""
    r = Rng64(state[0])
    v = r.next()
    state[0] = r.s
    return v


class SimBoard:
    """Headless game with the same semantics as the C Game struct."""

    def __init__(self, marks_enabled=True):
        self.marks_enabled = marks_enabled
        self.rows = 0
        self.cols = 0
        self.mines = 0
        self.difficulty = "beginner"
        self.rng = Rng64(0)
        self.seed = 0
        self.mine = []
        self.adj = []
        self.revealed = []
        self.mark = []
        self.opened = 0
        self.started = 0
        self.over = 0
        self.flags = 0
        self.time = 0
        self.paused = 0

    # ------------------------------------------------------------ lifecycle
    def _reset(self, rows, cols, mines, difficulty, seed):
        self.rows = rows
        self.cols = cols
        self.mines = mines
        self.difficulty = difficulty
        self.seed = normalise_seed(seed)
        self.rng = Rng64(self.seed)
        n = rows * cols
        self.mine = [False] * n
        self.adj = [0] * n
        self.revealed = [False] * n
        self.mark = [0] * n
        self.opened = 0
        self.started = 0
        self.over = 0
        self.flags = 0
        self.time = 0
        self.paused = 0

    def new(self, difficulty, seed):
        """difficulty: 'beginner' | 'intermediate' | 'expert' | 'custom r c m'."""
        parts = difficulty.split()
        diff = parts[0].lower()
        if diff == "custom" and len(parts) >= 4:
            rows = int(parts[1])
            cols = int(parts[2])
            mines = int(parts[3])
            self._reset(rows, cols, mines, "custom", seed)
        elif diff in PRESETS:
            rows, cols, mines = PRESETS[diff]
            self._reset(rows, cols, mines, diff, seed)
        else:
            raise ValueError("unknown difficulty: %r" % difficulty)
        return "OK"

    # ------------------------------------------------------------ protocol
    def command(self, line):
        """Handle one CLI-style command line; returns the full reply text."""
        toks = line.strip().split()
        if not toks:
            return "OK\nEND\n"
        cmd = toks[0].lower()
        try:
            if cmd == "new":
                if len(toks) >= 5 and toks[1].lower() == "custom":
                    self.new("custom %s %s %s" % (toks[2], toks[3], toks[4]),
                             self.seed)
                else:
                    self.new(toks[1] if len(toks) > 1 else "beginner", self.seed)
                return "OK\nEND\n"
            if cmd == "click":
                self.click(int(toks[1]), int(toks[2]))
                return "OK\nEND\n"
            if cmd == "flag":
                self.flag(int(toks[1]), int(toks[2]))
                return "OK\nEND\n"
            if cmd == "chord":
                self.chord(int(toks[1]), int(toks[2]))
                return "OK\nEND\n"
            if cmd == "refresh":
                return "OK\nEND\n"
            if cmd == "ping":
                return "OK\nEND\n"
            if cmd == "state":
                return self._state_text()
            if cmd == "board":
                return "\n".join(self.board()) + "\nEND\n"
        except (IndexError, ValueError):
            return "ERR bad args\nEND\n"
        return "ERR unknown command\nEND\n"

    def _state_text(self):
        return ("difficulty=%s\nrows=%d\ncols=%d\nmines=%d\nflags=%d\n"
                "opened=%d\ntime=%d\nstarted=%d\nover=%d\npaused=%d\n"
                "marks=%d\nseeded=1\nseed=%d\nEND\n" % (
                    self.difficulty, self.rows, self.cols, self.mines,
                    self.flags, self.opened, self.time, self.started,
                    self.over, self.paused, 1 if self.marks_enabled else 0,
                    self.seed))

    def state(self):
        out = {}
        for ln in self._state_text().splitlines():
            if ln == "END":
                continue
            if "=" in ln:
                k, v = ln.split("=", 1)
                out[k] = v
        return out

    # ------------------------------------------------------------ game core
    def _idx(self, r, c):
        return r * self.cols + c

    def _inb(self, r, c):
        return 0 <= r < self.rows and 0 <= c < self.cols

    def _compute_adj(self):
        for r in range(self.rows):
            for c in range(self.cols):
                cnt = 0
                for dr in (-1, 0, 1):
                    for dc in (-1, 0, 1):
                        if dr == 0 and dc == 0:
                            continue
                        rr, cc = r + dr, c + dc
                        if self._inb(rr, cc) and self.mine[self._idx(rr, cc)]:
                            cnt += 1
                self.adj[self._idx(r, c)] = cnt

    def _place_mines(self, sr, sc):
        pool = []
        for r in range(self.rows):
            for c in range(self.cols):
                if abs(r - sr) <= 1 and abs(c - sc) <= 1:
                    continue
                pool.append(self._idx(r, c))
        if len(pool) < self.mines:  # tiny board: only the clicked cell safe
            pool = [self._idx(r, c) for r in range(self.rows)
                    for c in range(self.cols) if not (r == sr and c == sc)]
        n = len(pool)
        placed = 0
        while placed < self.mines and n > 0:
            k = self.rng.next() % n
            idx = pool[k]
            pool[k] = pool[n - 1]
            n -= 1
            if not self.mine[idx]:
                self.mine[idx] = True
                placed += 1
        self._compute_adj()

    def _first_click(self, r, c):
        if self.started:
            return 0
        self.started = 1
        self._place_mines(r, c)
        return 1

    def _end_game_lose(self):
        if self.over:
            return
        self.over = -1
        for i in range(self.rows * self.cols):
            if self.mine[i]:
                self.revealed[i] = True

    def _end_game_win(self):
        if self.over:
            return
        self.over = 1
        for i in range(self.rows * self.cols):
            if self.mine[i] and self.mark[i] != 1:
                self.mark[i] = 1
                self.flags += 1

    def _reveal_cell(self, r, c):
        if not self._inb(r, c):
            return
        i = self._idx(r, c)
        if self.revealed[i] or self.mark[i] == 1:
            return
        if self.mine[i]:
            self._end_game_lose()
            return
        if self.over:
            return
        self.revealed[i] = True
        self.opened += 1
        if self.adj[i] == 0:
            for dr in (-1, 0, 1):
                for dc in (-1, 0, 1):
                    if dr == 0 and dc == 0:
                        continue
                    self._reveal_cell(r + dr, c + dc)
        if self.opened == self.rows * self.cols - self.mines:
            self._end_game_win()

    def click(self, r, c):
        if not self._inb(r, c):
            return
        if not self.started:
            self._first_click(r, c)
        self._reveal_cell(r, c)

    def _cycle_mark(self, cell):
        if self.over:
            return
        if self.mark[cell] == 0:
            self.mark[cell] = 1
            self.flags += 1
        elif self.mark[cell] == 1:
            self.flags -= 1
            self.mark[cell] = 2 if self.marks_enabled else 0
        else:
            self.mark[cell] = 0

    def flag(self, r, c):
        if self._inb(r, c):
            self._cycle_mark(self._idx(r, c))

    def _do_chord(self, cell):
        r = cell // self.cols
        c = cell % self.cols
        cnt = 0
        for dr in (-1, 0, 1):
            for dc in (-1, 0, 1):
                if dr == 0 and dc == 0:
                    continue
                rr, cc = r + dr, c + dc
                if self._inb(rr, cc) and self.mark[self._idx(rr, cc)] == 1:
                    cnt += 1
        if cnt == self.adj[cell]:
            for dr in (-1, 0, 1):
                for dc in (-1, 0, 1):
                    if dr == 0 and dc == 0:
                        continue
                    rr, cc = r + dr, c + dc
                    if self._inb(rr, cc):
                        self._reveal_cell(rr, cc)

    def chord(self, r, c):
        if self._inb(r, c):
            self._do_chord(self._idx(r, c))

    # ---------------------------------------------------------------- output
    def board(self):
        out = []
        for r in range(self.rows):
            row = []
            for c in range(self.cols):
                i = self._idx(r, c)
                if not self.revealed[i]:
                    row.append("F" if self.mark[i] == 1
                               else "?" if self.mark[i] == 2 else ".")
                elif self.mine[i]:
                    row.append("*")
                else:
                    row.append(chr(ord("0") + self.adj[i]))
            out.append("".join(row))
        return out


class SimSock:
    """Stand-in for a socket: sendall applies commands and queues replies."""

    def __init__(self, board):
        self.board = board
        self.queue = []

    def sendall(self, data):
        if isinstance(data, bytes):
            data = data.decode("ascii", "replace")
        for line in data.split("\n"):
            line = line.strip()
            if not line:
                continue
            reply = self.board.command(line)
            if reply:
                self.queue.extend(reply.splitlines())

    def close(self):
        pass


class SimClient:
    """MSClient-compatible adapter driving a SimBoard headlessly.

    play_game() in ms_solver.py talks to it exactly like a real client:
    new/click/flag/chord/state/board/refresh plus sock.sendall and
    _read_line() used by _batch().
    """

    def __init__(self, sim=None, seed=0, marks_enabled=True):
        self.sim = sim or SimBoard(marks_enabled=marks_enabled)
        self.sim.seed = seed
        self.sock = SimSock(self.sim)

    # ------------------------------------------------------------- MSClient
    def _read_line(self):
        if not self.sock.queue:
            raise ConnectionError("no pending reply (protocol error)")
        return self.sock.queue.pop(0)

    def cmd(self, text):
        self.sock.sendall(text + "\n")
        lines = []
        while True:
            line = self._read_line()
            if line == "END":
                return lines
            lines.append(line)

    def new(self, difficulty):
        return self.cmd("new " + difficulty)

    def click(self, r, c):
        return self.cmd("click %d %d" % (r, c))

    def flag(self, r, c):
        return self.cmd("flag %d %d" % (r, c))

    def chord(self, r, c):
        return self.cmd("chord %d %d" % (r, c))

    def state(self):
        return self.sim.state()

    def board(self):
        return self.sim.board()

    def refresh(self, on):
        return self.cmd("refresh %d" % (1 if on else 0))

    def close(self):
        pass


def board_for_seed(difficulty, seed, click=(None, None)):
    """Convenience: build a board for a seed and optionally apply a click."""
    b = SimBoard()
    b.new(difficulty, seed)
    if click != (None, None):
        b.click(*click)
    return b


if __name__ == "__main__":
    # tiny self-check when run directly
    b = board_for_seed("beginner", 12345, click=(3, 3))
    lost = b.over == -1
    print("rows=%d cols=%d mines=%d opened=%d over=%d lost=%s" %
          (b.rows, b.cols, b.mines, b.opened, b.over, lost))
    for ln in b.board():
        print(ln)
    sys.exit(0)
