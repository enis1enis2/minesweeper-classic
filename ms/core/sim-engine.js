// sim-engine.js - headless port of the C game's board generation and clicks.
//
// Implements, bit-for-bit, the board layout used by the real minesweeper.exe
// so simulated games and live games agree on the same seed:
//
//   * xorshift64 PRNG (minesweeper.c xorshift())
//   * place_mines(): pool of every cell outside the first click's 3x3, then a
//     partial Fisher-Yates draw using  k = rng() % n   while n shrinks; tiny
//     boards fall back to "only the clicked cell is safe".
//   * reveal_cell() flood fill with the auto-win / auto-flag-mines rules and
//     the auto-reveal-mines rule on loss.
//
// The board's click/flag/chord/board/state protocol matches the scripting CLI
// (minesweeper.c cli_*) so solver.js's play_game() drives it unmodified
// through the SimClient adapter.
//
// This is a 1:1 port of the Python sim_engine.py.  A single behavioral
// difference: 64-bit integers use BigInt (JS doubles cannot represent
// uint64), and the board arrays are Uint8Array instead of Python lists.

import path from "node:path";
import { fileURLToPath } from "node:url";

const MASK64 = (1n << 64n) - 1n;

// (rows, cols, mines) per difficulty, mirroring g_presets[].
export const PRESETS = {
  beginner: [8, 8, 10],
  intermediate: [16, 16, 40],
  expert: [16, 30, 99],
};

export const DEFAULTS = {
  beginner: "beginner",
  intermediate: "intermediate",
  expert: "expert",
};

// The game's xorshift64, advanced in place (one value per call).
export class Rng64 {
  constructor(seed) {
    this.s = BigInt(seed) & MASK64;
  }

  next() {
    let x = this.s;
    x ^= (x << 13n) & MASK64;
    x ^= x >> 7n;
    x ^= (x << 17n) & MASK64;
    x &= MASK64;
    this.s = x;
    return x;
  }
}

// Single-step convenience; state is a 1-element array (mutable holder).
export function xorshift(state) {
  const r = new Rng64(state[0]);
  const v = r.next();
  state[0] = r.s;
  return v;
}

function isBigInt(v) {
  return typeof v === "bigint";
}

// A tiny bit of conversion glue: every public entry accepts either a Number
// or a BigInt seed and normalises to BigInt (matching Python's `seed & MASK64`).
function toU64(seed) {
  return BigInt(seed) & MASK64;
}

export class SimBoard {
  constructor(marksEnabled = true) {
    this.marks_enabled = marksEnabled;
    this.rows = 0;
    this.cols = 0;
    this.mines = 0;
    this.difficulty = "beginner";
    this.rng = new Rng64(0n);
    this.seed = 0n;
    this.mine = [];
    this.adj = [];
    this.revealed = [];
    this.mark = [];
    this.opened = 0;
    this.started = 0;
    this.over = 0;
    this.flags = 0;
    this.time = 0;
    this.paused = 0;
  }

  _reset(rows, cols, mines, difficulty, seed) {
    this.rows = rows;
    this.cols = cols;
    this.mines = mines;
    this.difficulty = difficulty;
    this.seed = toU64(seed);
    this.rng = new Rng64(this.seed);
    const n = rows * cols;
    this.mine = new Uint8Array(n);
    this.adj = new Uint8Array(n);
    this.revealed = new Uint8Array(n);
    this.mark = new Uint8Array(n);
    this.opened = 0;
    this.started = 0;
    this.over = 0;
    this.flags = 0;
    this.time = 0;
    this.paused = 0;
  }

  // difficulty: 'beginner' | 'intermediate' | 'expert' | 'custom r c m'.
  new(difficulty, seed) {
    const parts = String(difficulty).split(/\s+/);
    const diff = parts[0].toLowerCase();
    if (diff === "custom" && parts.length >= 4) {
      const rows = parseInt(parts[1], 10);
      const cols = parseInt(parts[2], 10);
      const mines = parseInt(parts[3], 10);
      this._reset(rows, cols, mines, "custom", seed);
    } else if (Object.prototype.hasOwnProperty.call(PRESETS, diff)) {
      const [rows, cols, mines] = PRESETS[diff];
      this._reset(rows, cols, mines, diff, seed);
    } else {
      throw new Error("unknown difficulty: " + JSON.stringify(difficulty));
    }
    return "OK";
  }

  // Handle one CLI-style command line; returns the full reply text.
  command(line) {
    const toks = String(line).trim().split(/\s+/);
    if (toks.length === 0 || (toks.length === 1 && toks[0] === "")) {
      return "OK\nEND\n";
    }
    const cmd = toks[0].toLowerCase();
    try {
      if (cmd === "new") {
        if (toks.length >= 5 && toks[1].toLowerCase() === "custom") {
          this.new(
            `custom ${toks[2]} ${toks[3]} ${toks[4]}`,
            this.seed,
          );
        } else {
          this.new(toks.length > 1 ? toks[1] : "beginner", this.seed);
        }
        return "OK\nEND\n";
      }
      if (cmd === "click") {
        this.click(parseInt(toks[1], 10), parseInt(toks[2], 10));
        return "OK\nEND\n";
      }
      if (cmd === "flag") {
        this.flag(parseInt(toks[1], 10), parseInt(toks[2], 10));
        return "OK\nEND\n";
      }
      if (cmd === "chord") {
        this.chord(parseInt(toks[1], 10), parseInt(toks[2], 10));
        return "OK\nEND\n";
      }
      if (cmd === "refresh" || cmd === "ping") {
        return "OK\nEND\n";
      }
      if (cmd === "state") {
        return this._stateText();
      }
      if (cmd === "board") {
        return this.board().join("\n") + "\nEND\n";
      }
    } catch {
      return "ERR bad args\nEND\n";
    }
    return "ERR unknown command\nEND\n";
  }

  _stateText() {
    return [
      `difficulty=${this.difficulty}`,
      `rows=${this.rows}`,
      `cols=${this.cols}`,
      `mines=${this.mines}`,
      `flags=${this.flags}`,
      `opened=${this.opened}`,
      `time=${this.time}`,
      `started=${this.started}`,
      `over=${this.over}`,
      `paused=${this.paused}`,
      `marks=${this.marks_enabled ? 1 : 0}`,
      `seeded=1`,
      `seed=${this.seed}`,
      "END",
    ].join("\n") + "\n";
  }

  state() {
    const out = {};
    for (const ln of this._stateText().split("\n")) {
      if (ln === "END") continue;
      const eq = ln.indexOf("=");
      if (eq >= 0) out[ln.slice(0, eq)] = ln.slice(eq + 1);
    }
    return out;
  }

  _idx(r, c) {
    return r * this.cols + c;
  }

  _inb(r, c) {
    return r >= 0 && r < this.rows && c >= 0 && c < this.cols;
  }

  _computeAdj() {
    const { rows, cols } = this;
    for (let r = 0; r < rows; r++) {
      for (let c = 0; c < cols; c++) {
        let cnt = 0;
        for (let dr = -1; dr <= 1; dr++) {
          for (let dc = -1; dc <= 1; dc++) {
            if (dr === 0 && dc === 0) continue;
            const rr = r + dr;
            const cc = c + dc;
            if (this._inb(rr, cc) && this.mine[this._idx(rr, cc)]) cnt++;
          }
        }
        this.adj[this._idx(r, c)] = cnt;
      }
    }
  }

  _placeMines(sr, sc) {
    const pool = [];
    for (let r = 0; r < this.rows; r++) {
      for (let c = 0; c < this.cols; c++) {
        if (Math.abs(r - sr) <= 1 && Math.abs(c - sc) <= 1) continue;
        pool.push(this._idx(r, c));
      }
    }
    if (pool.length < this.mines) {
      // tiny board: only the clicked cell safe
      pool.length = 0;
      for (let r = 0; r < this.rows; r++) {
        for (let c = 0; c < this.cols; c++) {
          if (!(r === sr && c === sc)) pool.push(this._idx(r, c));
        }
      }
    }
    let n = pool.length;
    let placed = 0;
    while (placed < this.mines && n > 0) {
      const k = Number(this.rng.next() % BigInt(n));
      const idx = pool[k];
      pool[k] = pool[n - 1];
      n -= 1;
      if (!this.mine[idx]) {
        this.mine[idx] = 1;
        placed += 1;
      }
    }
    this._computeAdj();
  }

  _firstClick(r, c) {
    if (this.started) return 0;
    this.started = 1;
    this._placeMines(r, c);
    return 1;
  }

  _endGameLose() {
    if (this.over) return;
    this.over = -1;
    for (let i = 0; i < this.rows * this.cols; i++) {
      if (this.mine[i]) this.revealed[i] = 1;
    }
  }

  _endGameWin() {
    if (this.over) return;
    this.over = 1;
    for (let i = 0; i < this.rows * this.cols; i++) {
      if (this.mine[i] && this.mark[i] !== 1) {
        this.mark[i] = 1;
        this.flags += 1;
      }
    }
  }

  _revealCell(r, c) {
    if (!this._inb(r, c)) return;
    const i = this._idx(r, c);
    if (this.revealed[i] || this.mark[i] === 1) return;
    if (this.mine[i]) {
      this._endGameLose();
      return;
    }
    if (this.over) return;
    this.revealed[i] = 1;
    this.opened += 1;
    if (this.adj[i] === 0) {
      for (let dr = -1; dr <= 1; dr++) {
        for (let dc = -1; dc <= 1; dc++) {
          if (dr === 0 && dc === 0) continue;
          this._revealCell(r + dr, c + dc);
        }
      }
    }
    if (this.opened === this.rows * this.cols - this.mines) {
      this._endGameWin();
    }
  }

  click(r, c) {
    if (!this._inb(r, c)) return;
    if (!this.started) this._firstClick(r, c);
    this._revealCell(r, c);
  }

  _cycleMark(cell) {
    if (this.over) return;
    if (this.mark[cell] === 0) {
      this.mark[cell] = 1;
      this.flags += 1;
    } else if (this.mark[cell] === 1) {
      this.flags -= 1;
      this.mark[cell] = this.marks_enabled ? 2 : 0;
    } else {
      this.mark[cell] = 0;
    }
  }

  flag(r, c) {
    if (this._inb(r, c)) this._cycleMark(this._idx(r, c));
  }

  _doChord(cell) {
    const r = Math.floor(cell / this.cols);
    const c = cell % this.cols;
    let cnt = 0;
    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;
        const rr = r + dr;
        const cc = c + dc;
        if (this._inb(rr, cc) && this.mark[this._idx(rr, cc)] === 1) cnt++;
      }
    }
    if (cnt === this.adj[cell]) {
      for (let dr = -1; dr <= 1; dr++) {
        for (let dc = -1; dc <= 1; dc++) {
          if (dr === 0 && dc === 0) continue;
          const rr = r + dr;
          const cc = c + dc;
          if (this._inb(rr, cc)) this._revealCell(rr, cc);
        }
      }
    }
  }

  chord(r, c) {
    if (this._inb(r, c)) this._doChord(this._idx(r, c));
  }

  board() {
    const out = [];
    for (let r = 0; r < this.rows; r++) {
      let row = "";
      for (let c = 0; c < this.cols; c++) {
        const i = this._idx(r, c);
        if (!this.revealed[i]) {
          row += this.mark[i] === 1 ? "F" : this.mark[i] === 2 ? "?" : ".";
        } else if (this.mine[i]) {
          row += "*";
        } else {
          row += String.fromCharCode(48 + this.adj[i]);
        }
      }
      out.push(row);
    }
    return out;
  }
}

// Stand-in for a socket: sendall applies commands and queues replies.
export class SimSock {
  constructor(board) {
    this.board = board;
    this.queue = [];
  }

  sendall(data) {
    let text = data;
    if (typeof text !== "string") {
      text = Buffer.from(text).toString("ascii");
    }
    for (const rawLine of text.split("\n")) {
      const line = rawLine.trim();
      if (!line) continue;
      const reply = this.board.command(line);
      if (reply) {
        for (const l of reply.split("\n")) this.queue.push(l);
      }
    }
  }

  close() {}
}

// SimClient - solver.js-compatible adapter driving a SimBoard headlessly.
export class SimClient {
  constructor(sim = null, seed = 0, marksEnabled = true) {
    this.sim = sim || new SimBoard(marksEnabled);
    this.sim.seed = isBigInt(seed) ? seed : BigInt(seed);
    this.sock = new SimSock(this.sim);
  }

  _readLine() {
    if (this.sock.queue.length === 0) {
      throw new Error("no pending reply (protocol error)");
    }
    return this.sock.queue.shift();
  }

  cmd(text) {
    this.sock.sendall(text + "\n");
    const lines = [];
    while (true) {
      const line = this._readLine();
      if (line === "END") return lines;
      lines.push(line);
    }
  }

  new(difficulty) {
    return this.cmd("new " + difficulty);
  }

  click(r, c) {
    return this.cmd(`click ${r} ${c}`);
  }

  flag(r, c) {
    return this.cmd(`flag ${r} ${c}`);
  }

  chord(r, c) {
    return this.cmd(`chord ${r} ${c}`);
  }

  state() {
    return this.sim.state();
  }

  board() {
    return this.sim.board();
  }

  refresh(on) {
    return this.cmd(`refresh ${on ? 1 : 0}`);
  }

  close() {}
}

export function boardForSeed(difficulty, seed, click = null) {
  const b = new SimBoard();
  b.new(difficulty, seed);
  if (click !== null && click[0] !== null && click[1] !== null) {
    b.click(click[0], click[1]);
  }
  return b;
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  // tiny self-check when run directly (mirrors sim_engine.py __main__)
  const b = boardForSeed("beginner", 12345, [3, 3]);
  const lost = b.over === -1;
  console.log(
    `rows=${b.rows} cols=${b.cols} mines=${b.mines} opened=${b.opened} over=${b.over} lost=${lost}`,
  );
  for (const ln of b.board()) console.log(ln);
  process.exit(0);
}
