// solver.js - deterministic Minesweeper player.
//
// 1:1 port of the Python minesweeper_bot/ms_solver.py.  Drives a game through
// the client adapter (SimClient for headless sims, MSClient for the network
// server) with a constraint-propagation deduction pass plus an exact
// probabilistic pass over the frontier of unrevealed cells.
//
// Determinism: Python's frozenset/set iteration order is an implementation
// detail, so the JS port deliberately uses insertion-ordered Set/Map instead.
// This keeps a game deterministic for a given RNG seed within the language,
// but a JS game is not expected to reproduce the exact same move sequence as
// the Python original (probabilities are still computed exactly).

import { Random } from "./mt19937.js";

// math.comb(n, k) via BigInt so large binomial coefficients never overflow.
function combBig(n, k) {
  if (k < 0 || k > n) return 0n;
  if (k > n - k) k = n - k;
  let c = 1n;
  for (let i = 1; i <= k; i++) {
    c = (c * BigInt(n - k + i)) / BigInt(i);
  }
  return c;
}

export class Board {
  constructor(rows, lines, totalMines) {
    this.rows = rows;
    this.lines = lines.map((ln) => ln.replace(/\r$/, ""));
    this.cols = rows ? this.lines[0].length : 0;
    this.totalMines = totalMines;
    const n = rows * this.cols;
    this.revealed = new Array(n).fill(false);
    this.mine = new Array(n).fill(false);
    this.flagged = new Array(n).fill(false);
    this.q = new Array(n).fill(false);
    this.num = new Array(n).fill(0);
    this._parse();
  }

  _parse() {
    for (let r = 0; r < this.lines.length; r++) {
      const ln = this.lines[r];
      for (let c = 0; c < ln.length; c++) {
        const i = r * this.cols + c;
        const ch = ln[c];
        if (ch === "F") {
          this.flagged[i] = true;
        } else if (ch === "?") {
          this.q[i] = true;
        } else if (ch === "*") {
          this.revealed[i] = true;
          this.mine[i] = true;
        } else if (ch >= "0" && ch <= "8") {
          this.revealed[i] = true;
          this.num[i] = ch.charCodeAt(0) - 48;
        }
      }
    }
  }

  id(r, c) {
    return r * this.cols + c;
  }

  rc(i) {
    return [Math.floor(i / this.cols), i % this.cols];
  }

  neighbors(r, c) {
    const out = [];
    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;
        const nr = r + dr;
        const nc = c + dc;
        if (nr >= 0 && nr < this.rows && nc >= 0 && nc < this.cols) {
          out.push(this.id(nr, nc));
        }
      }
    }
    return out;
  }

  hidden(i) {
    return !this.revealed[i] && !this.flagged[i];
  }

  flagsAround(i) {
    const [r, c] = this.rc(i);
    let sum = 0;
    for (const j of this.neighbors(r, c)) if (this.flagged[j]) sum++;
    return sum;
  }

  hiddenAround(i) {
    const [r, c] = this.rc(i);
    return this.neighbors(r, c).filter((j) => this.hidden(j));
  }

  allHidden() {
    const out = [];
    for (let i = 0; i < this.rows * this.cols; i++) {
      if (this.hidden(i)) out.push(i);
    }
    return out;
  }

  lost() {
    return this.mine.some(Boolean);
  }

  won() {
    if (this.lost()) return false;
    for (const ln of this.lines) {
      for (const ch of ln) {
        if (ch === "." || ch === "?") return false;
      }
    }
    return true;
  }
}

// ---------------------------------------------------------------------
// deterministic deduction
// ---------------------------------------------------------------------
export function buildConstraints(board) {
  const cons = [];
  for (let r = 0; r < board.rows; r++) {
    for (let c = 0; c < board.cols; c++) {
      const i = board.id(r, c);
      if (!board.revealed[i] || board.mine[i]) continue;
      const hidden = board.hiddenAround(i);
      if (!hidden.length) continue;
      const need = board.num[i] - board.flagsAround(i);
      if (need < 0 || need > hidden.length) continue;
      cons.push({ cells: new Set(hidden), need });
    }
  }
  return cons;
}

export function deduce(board, cons) {
  const safes = new Set();
  const mines = new Set();

  const addSafe = (s) => {
    for (const i of s) if (board.hidden(i)) safes.add(i);
  };
  const addMine = (s) => {
    for (const i of s) if (board.hidden(i)) mines.add(i);
  };

  for (const { cells, need } of cons) {
    if (need === 0) addSafe(cells);
    else if (need === cells.size) addMine(cells);
  }

  const subset = (a, b) => {
    if (a.size > b.size) return false;
    for (const x of a) if (!b.has(x)) return false;
    return true;
  };

  let changed = true;
  while (changed) {
    changed = false;
    for (const A of cons) {
      for (const B of cons) {
        if (A === B || !subset(A.cells, B.cells)) continue;
        const diff = [];
        for (const x of B.cells) if (!A.cells.has(x)) diff.push(x);
        const dneed = B.need - A.need;
        if (dneed === 0 && diff.length) {
          const fresh = diff.filter((i) => board.hidden(i));
          if (fresh.length && !fresh.every((i) => safes.has(i))) {
            for (const i of fresh) safes.add(i);
            changed = true;
          }
        } else if (dneed === diff.length && diff.length) {
          const fresh = diff.filter((i) => board.hidden(i));
          if (fresh.length && !fresh.every((i) => mines.has(i))) {
            for (const i of fresh) mines.add(i);
            changed = true;
          }
        }
      }
    }
  }
  return { safes, mines };
}

// ---------------------------------------------------------------------
// exact frontier probability
// ---------------------------------------------------------------------
export function frontierComponents(board, cons) {
  const cells = new Set();
  for (const { cells: s } of cons) for (const i of s) cells.add(i);
  if (!cells.size) return { cells, comps: [] };

  const parent = new Map();
  for (const i of cells) parent.set(i, i);

  const find = (x) => {
    while (parent.get(x) !== x) {
      parent.set(x, parent.get(parent.get(x)));
      x = parent.get(x);
    }
    return x;
  };
  const union = (a, b) => {
    const ra = find(a);
    const rb = find(b);
    if (ra !== rb) parent.set(ra, rb);
  };

  for (const { cells: s } of cons) {
    const sl = [...s];
    for (let i = 1; i < sl.length; i++) union(sl[0], sl[i]);
  }

  const comps = new Map();
  for (const i of cells) {
    const root = find(i);
    if (!comps.has(root)) comps.set(root, []);
    comps.get(root).push(i);
  }
  return { cells, comps: [...comps.values()] };
}

export function solveComponent(localCells, localCons, nodeBudget = 2000000) {
  const m = localCells.length;
  if (m === 0) return { S: new Map([[0, 1]]), T: [] };
  const cellPos = new Map();
  for (let i = 0; i < m; i++) cellPos.set(localCells[i], i);
  const cons = localCons.map(({ cells, need }) => {
    const idx = [];
    for (const c of cells) idx.push(cellPos.get(c));
    return { idx, need };
  });

  const member = [];
  for (let i = 0; i < m; i++) member.push([]);
  cons.forEach(({ idx }, ci) => {
    for (const li of idx) member[li].push(ci);
  });

  const order = [...Array(m).keys()].sort(
    (a, b) => -member[a].length - -member[b].length || a - b,
  );
  const orderPos = new Array(m).fill(0);
  order.forEach((li, p) => {
    orderPos[li] = p;
  });

  const assigned = new Array(m).fill(-1);
  const conMines = new Array(cons.length).fill(0);
  const conLeft = cons.map(({ idx }) => idx.length);

  const S = new Map();
  const T = [];
  for (let i = 0; i < m; i++) T.push(new Map());
  const nodes = [0];

  const feasible = (li, val) => {
    for (const ci of member[li]) {
      const need = cons[ci].need;
      const newMines = conMines[ci] + val;
      if (newMines > need) return false;
      const newLeft = conLeft[ci] - 1;
      if (need - newMines > newLeft) return false;
    }
    return true;
  };

  const rec = (p) => {
    nodes[0]++;
    if (nodes[0] > nodeBudget) return;
    if (p === m) {
      let total = 0;
      for (let li = 0; li < m; li++) if (assigned[li] === 1) total++;
      S.set(total, (S.get(total) || 0) + 1);
      for (let li = 0; li < m; li++) {
        if (assigned[li] === 1) {
          T[li].set(total, (T[li].get(total) || 0) + 1);
        }
      }
      return;
    }
    const li = order[p];
    for (const val of [0, 1]) {
      if (!feasible(li, val)) continue;
      assigned[li] = val;
      for (const ci of member[li]) {
        conMines[ci] += val;
        conLeft[ci] -= 1;
      }
      rec(p + 1);
      for (const ci of member[li]) {
        conMines[ci] -= val;
        conLeft[ci] += 1;
      }
      assigned[li] = -1;
    }
  };

  rec(0);
  if (nodes[0] > nodeBudget) return null;
  return { S, T };
}

function convolve(d1, d2) {
  const out = new Map();
  for (const [k1, v1] of d1) {
    for (const [k2, v2] of d2) {
      const k = k1 + k2;
      out.set(k, (out.get(k) || 0) + v1 * v2);
    }
  }
  return out;
}

export function frontierProbabilities(board, cons, nodeBudget = 2000000) {
  const { cells, comps } = frontierComponents(board, cons);
  if (!cells.size) return { probs: new Map(), nonfrontierP: null };

  const allS = cells;
  const free = [];
  for (let i = 0; i < board.rows * board.cols; i++) {
    if (board.hidden(i) && !allS.has(i)) free.push(i);
  }
  const nFree = free.length;

  const compData = [];
  for (const comp of comps) {
    const compSet = new Set(comp);
    const localCons = cons.filter((c) => {
      for (const x of c.cells) if (!compSet.has(x)) return false;
      return true;
    });
    const res = solveComponent(comp, localCons, nodeBudget);
    if (res === null) continue;
    compData.push({ comp, S: res.S, T: res.T });
  }
  if (!compData.length) {
    return { probs: new Map(), nonfrontierP: null };
  }

  const compsDist = [];
  const compsT = [];
  const compsCells = [];
  const compsTot = [];
  for (const { comp, S, T } of compData) {
    let tot = 0;
    for (const v of S.values()) tot += v;
    const d = new Map();
    for (const [k, v] of S) d.set(k, v / tot);
    compsDist.push(d);
    compsT.push(T);
    compsCells.push(comp);
    compsTot.push(tot);
  }

  const k = compsDist.length;
  const prefix = [];
  prefix[0] = new Map([[0, 1.0]]);
  for (let i = 0; i < k; i++) prefix[i + 1] = convolve(prefix[i], compsDist[i]);
  const suffix = [];
  suffix[k] = new Map([[0, 1.0]]);
  for (let i = k - 1; i >= 0; i--) suffix[i] = convolve(compsDist[i], suffix[i + 1]);

  const M = board.totalMines;
  const D = prefix[k];

  const raw = new Map();
  for (const t of D.keys()) raw.set(t, combBig(nFree, M - t));
  let mx = 0n;
  for (const v of raw.values()) if (v > mx) mx = v;
  let w;
  if (mx <= 0n) {
    w = new Map();
    for (const t of D.keys()) w.set(t, 1.0);
  } else {
    w = new Map();
    for (const [t, v] of raw) w.set(t, Number(v) / Number(mx));
  }

  let Z = 0;
  for (const [t, dt] of D) Z += dt * w.get(t);
  if (Z <= 0) return { probs: new Map(), nonfrontierP: null };

  let EFront = 0;
  for (const [t, dt] of D) EFront += t * dt * w.get(t);
  EFront /= Z;

  let nonfrontierP = 0.0;
  if (nFree > 0) {
    nonfrontierP = Math.max(0.0, Math.min(1.0, (M - EFront) / nFree));
  }

  const probs = new Map();
  for (let ci = 0; ci < compsCells.length; ci++) {
    const comp = compsCells[ci];
    const DExcept = convolve(prefix[ci], suffix[ci + 1]);
    const T = compsT[ci];
    const totI = compsTot[ci];
    for (let li = 0; li < comp.length; li++) {
      const cell = comp[li];
      let num = 0.0;
      for (const [total, cnt] of T[li]) {
        let u = 0.0;
        for (const [o, po] of DExcept) {
          u += po * (w.get(total + o) || 0.0);
        }
        num += (cnt / totI) * u;
      }
      probs.set(cell, num / Z);
    }
  }
  return { probs, nonfrontierP };
}

// ---------------------------------------------------------------------
// move selection
// ---------------------------------------------------------------------
export function chooseMove(board, probs, nonfrontierP, rng, strategy) {
  const tie = strategy.get("tiebreak") ?? "minprob";
  const frontier = new Set(probs.keys());
  const free = [];
  for (let i = 0; i < board.rows * board.cols; i++) {
    if (board.hidden(i) && !frontier.has(i)) free.push(i);
  }

  const candidates = [];
  for (const [i, p] of probs) candidates.push([p, i, "frontier"]);
  if (free.length && nonfrontierP !== null) {
    for (const i of free) candidates.push([nonfrontierP, i, "free"]);
  }

  if (!candidates.length) {
    const hidden = board.allHidden();
    return hidden.length ? rng.choice(hidden) : null;
  }

  let minp = Infinity;
  for (const [p] of candidates) if (p < minp) minp = p;
  const best = candidates.filter((c) => c[0] <= minp + 1e-12);

  if (tie === "random") {
    return rng.choice(best)[1];
  }

  const info = (c) => {
    const [p, i, kind] = c;
    if (kind === "free") return [2.0, rng.random()];
    const [r, cc] = board.rc(i);
    const nbrs = board.neighbors(r, cc);
    let rev = 0;
    for (const j of nbrs) if (board.revealed[j] && !board.mine[j]) rev++;
    return [(9 - rev) / 9.0, rng.random()];
  };

  let bestC = best[0];
  let bestKey = info(bestC);
  for (let idx = 1; idx < best.length; idx++) {
    const key = info(best[idx]);
    if (key[0] > bestKey[0]) {
      bestKey = key;
      bestC = best[idx];
    }
  }
  return bestC[1];
}

// ---------------------------------------------------------------------
// the player
// ---------------------------------------------------------------------
export function playGame(client, difficulty, strategy, rng, stats = null) {
  const refreshOn = strategy.get("refresh") ?? true;
  if (!refreshOn) client.refresh(false);
  client.new(difficulty);
  const st = client.state();
  const rows = parseInt(st.rows, 10);
  const cols = parseInt(st.cols, 10);
  const mines = parseInt(st.mines, 10);

  const first = strategy.get("first") ?? "center";
  let r0, c0;
  if (first === "corner") {
    r0 = 0;
    c0 = 0;
  } else {
    r0 = Math.floor(rows / 2);
    c0 = Math.floor(cols / 2);
  }
  client.click(r0, c0);

  let moves = 1;
  let chords = 0;
  let flagsPlaced = 0;
  let guesses = 0;
  let deduceBatches = 0;
  const guessSamples = [];

  while (true) {
    const board = new Board(rows, client.board(), mines);
    if (board.lost() || board.won()) break;

    const cons = buildConstraints(board);
    const { safes, mines: minesSet } = deduce(board, cons);

    if (
      safes.size ||
      minesSet.size ||
      ((strategy.get("use_chord") ?? true) && chordable(board))
    ) {
      deduceBatches += 1;
      const cmds = [];
      for (const i of minesSet) {
        const [r, c] = board.rc(i);
        cmds.push(["flag", r, c]);
      }
      if (strategy.get("use_chord") ?? true) {
        for (let r = 0; r < board.rows; r++) {
          for (let c = 0; c < board.cols; c++) {
            const i = board.id(r, c);
            if (!board.revealed[i] || board.mine[i]) continue;
            if (board.num[i] === 0) continue;
            if (board.flagsAround(i) === board.num[i]) {
              if (board.hiddenAround(i).length) {
                cmds.push(["chord", r, c]);
              }
            }
          }
        }
      }
      for (const i of safes) {
        if (!minesSet.has(i)) {
          const [r, c] = board.rc(i);
          cmds.push(["click", r, c]);
        }
      }
      flagsPlaced += minesSet.size;
      chords += cmds.filter((c) => c[0] === "chord").length;
      moves += cmds.length;
      batch(client, cmds);
      continue;
    }

    const { probs, nonfrontierP } = frontierProbabilities(board, cons);
    const target = chooseMove(board, probs, nonfrontierP, rng, strategy);
    if (target === null) break;
    const [r, c] = board.rc(target);
    const probsSet = new Set(probs.keys());
    guessSamples.push([
      probs.size,
      (() => {
        let cnt = 0;
        for (let i = 0; i < rows * cols; i++) {
          if (board.hidden(i) && !probsSet.has(i)) cnt++;
        }
        return cnt;
      })(),
      probs.has(target) ? probs.get(target) : nonfrontierP,
    ]);
    client.click(r, c);
    moves += 1;
    guesses += 1;
  }

  const board = new Board(rows, client.board(), mines);
  const st2 = client.state();
  const res = {
    win: board.won() && !board.lost(),
    time: parseInt(st2.time ?? "0", 10),
    moves,
    chords,
    flags: flagsPlaced,
    guesses,
    deduce_batches: deduceBatches,
    frontier: guessSamples,
    difficulty,
  };
  if (stats !== null) {
    stats.games = (stats.games || 0) + 1;
    stats.wins = (stats.wins || 0) + (res.win ? 1 : 0);
    stats.total_guesses = (stats.total_guesses || 0) + guesses;
    stats.total_moves = (stats.total_moves || 0) + moves;
    stats.frontier_samples = (stats.frontier_samples || 0) + guessSamples.length;
    if (!stats.guess_p_sums) stats.guess_p_sums = [0.0, 0];
    let ps = 0;
    for (const [_f, _n, p] of guessSamples) ps += p;
    stats.guess_p_sums[0] += ps;
    stats.guess_p_sums[1] += guessSamples.length;
  }
  return res;
}

function chordable(board) {
  for (let r = 0; r < board.rows; r++) {
    for (let c = 0; c < board.cols; c++) {
      const i = board.id(r, c);
      if (!board.revealed[i] || board.mine[i]) continue;
      if (board.num[i] === 0) continue;
      if (board.flagsAround(i) === board.num[i] && board.hiddenAround(i).length) {
        return true;
      }
    }
  }
  return false;
}

function batch(client, cmds) {
  const payload = cmds.map(([cmd, r, c]) => `${cmd} ${r} ${c}`).join("\n") + "\n";
  client.sock.sendall(payload);
  for (const _cmd of cmds) {
    while (true) {
      const line = client._readLine();
      if (line === "END") break;
    }
  }
}

export { Random };
