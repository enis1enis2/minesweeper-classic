// analyze-diff.js - differential test: C engine vs Node solver.
//
// Zero-dependency port of tools/analyze_diff.py.  Generates random mid-game
// boards, runs both the C harness (build/analyze_test.exe) and the Node
// reference (analyze-ref.js) on each, and reports any cell whose P(mine)
// differs by more than 1e-9 or whose reveal-count differs.
//
// Windows only: needs build/analyze_test.exe (built by tools/run_analyze_diff.cmd).

import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Random } from "../core/mt19937.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "..", "..");
const C_TOOL = path.join(REPO, "build", "analyze_test.exe");
const REF_JS = path.join(HERE, "analyze-ref.js");

// Python random.sample(range(n), k) equivalent: partial Fisher-Yates using the
// given Random instance (self-consistent, deterministic for a given seed).
function sample(rng, n, k) {
  const pool = [];
  for (let i = 0; i < n; i++) pool.push(i);
  const out = [];
  for (let i = 0; i < k; i++) {
    const j = i + Number(rng._randbelow(n - i));
    const tmp = pool[i];
    pool[i] = pool[j];
    pool[j] = tmp;
    out.push(pool[i]);
  }
  return out;
}

export function genBoard(rng, rows, cols) {
  // Return (mines_total, board_rows) for a random mid-game state.
  const n = rows * cols;
  const minesTotal = rng.randint(Math.max(2, Math.floor(n / 12)), Math.floor(n / 6));
  const mines = new Set(sample(rng, n, minesTotal));

  // reveal a random blob of non-mine cells around a centre that is not a mine
  let centre = rng.randrange(n);
  while (mines.has(centre)) centre = rng.randrange(n);

  const revealed = new Set([centre]);
  const frontier = [centre];
  while (frontier.length) {
    const x = frontier.pop();
    if (revealed.size > Math.floor(n / 3)) break;
    const r = Math.floor(x / cols);
    const c = x % cols;
    const nbrs = [];
    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;
        const nr = r + dr;
        const nc = c + dc;
        if (nr >= 0 && nr < rows && nc >= 0 && nc < cols) {
          nbrs.push(nr * cols + nc);
        }
      }
    }
    for (const y of nbrs) {
      if (mines.has(y) || revealed.has(y)) continue;
      if (rng.random() < 0.5) {
        revealed.add(y);
        frontier.push(y);
      }
    }
  }

  const nbrMines = (r, c) => {
    let cnt = 0;
    for (let dr = -1; dr <= 1; dr++) {
      for (let dc = -1; dc <= 1; dc++) {
        if (dr === 0 && dc === 0) continue;
        const nr = r + dr;
        const nc = c + dc;
        if (nr >= 0 && nr < rows && nc >= 0 && nc < cols && mines.has(nr * cols + nc)) {
          cnt += 1;
        }
      }
    }
    return cnt;
  };

  const flagged = new Set();
  // flag a few hidden cells next to numbers to exercise need != adj
  const hid = [];
  for (let i = 0; i < n; i++) if (!revealed.has(i) && !mines.has(i)) hid.push(i);
  rng.shuffle(hid);
  const nFlags = rng.randint(0, Math.min(4, hid.length));
  for (const i of hid.slice(0, nFlags)) {
    const r = Math.floor(i / cols);
    const c = i % cols;
    const touching = (() => {
      for (let dr = -1; dr <= 1; dr++) {
        for (let dc = -1; dc <= 1; dc++) {
          if (dr === 0 && dc === 0) continue;
          const nr = r + dr;
          const nc = c + dc;
          if (nr >= 0 && nr < rows && nc >= 0 && nc < cols && revealed.has(nr * cols + nc)) {
            return true;
          }
        }
      }
      return false;
    })();
    if (touching) flagged.add(i);
  }

  const board = [];
  for (let r = 0; r < rows; r++) {
    let line = "";
    for (let c = 0; c < cols; c++) {
      const i = r * cols + c;
      if (mines.has(i)) line += "."; // mine stays hidden during play
      else if (flagged.has(i)) line += "F";
      else if (revealed.has(i)) line += String(nbrMines(r, c));
      else line += ".";
    }
    board.push(line);
  }
  return [minesTotal, board];
}

export function runC(tool, text) {
  try {
    const out = execFileSync(tool, [], { input: text, encoding: "ascii", timeout: 60000 });
    return { rc: 0, out };
  } catch (e) {
    if (e.code === "ENOENT") return { rc: 127, out: "" };
    const rc = typeof e.status === "number" ? e.status : 1;
    return { rc, out: (e.stdout || "") + (e.stderr || "") };
  }
}

export function runRef(nodeBin, refJs, text) {
  try {
    const out = execFileSync(nodeBin, [refJs], {
      input: text,
      encoding: "ascii",
      timeout: 60000,
    });
    return { rc: 0, out };
  } catch (e) {
    const rc = typeof e.status === "number" ? e.status : 1;
    return { rc, out: (e.stdout || "") + (e.stderr || "") };
  }
}

export function compareBoards(text, tool, nodeBin, refJs, t) {
  const { rc: rcC, out: outC } = runC(tool, text);
  const { rc: rcP, out: outP } = runRef(nodeBin, refJs, text);

  if (rcC !== 0 || rcP !== 0) {
    console.log(`=== board ${t} (rc c=${rcC} ref=${rcP}) ===`);
    console.log(text);
    console.log("C  :", outC.trim());
    console.log("REF:", outP.trim());
    return false;
  }

  const linesC = outC.split(/\r?\n/).filter((l) => l && !l.startsWith("#"));
  const linesP = outP.split(/\r?\n/).filter((l) => l && !l.startsWith("#"));
  if (linesC.length !== linesP.length) {
    console.log(`=== board ${t}: line count ${linesC.length} vs ${linesP.length} ===`);
    console.log(text);
    return false;
  }

  for (let i = 0; i < linesC.length; i++) {
    const a = linesC[i].split(/\s+/);
    const b = linesP[i].split(/\s+/);
    const ca = [Number(a[0]), Number(a[1]), Number(a[2]), Number(a[3])];
    const cb = [Number(b[0]), Number(b[1]), Number(b[2]), Number(b[3])];
    const ia = Number(a[5]);
    const ib = Number(b[5]);
    if (Math.abs(ca[2] - cb[2]) > 1e-9 || Math.abs(ca[3] - cb[3]) > 1e-9 || ia !== ib) {
      console.log(`=== board ${t} mismatch cell ${a[0]} ===`);
      console.log(text);
      console.log("C  :", linesC[i]);
      console.log("REF:", linesP[i]);
      return false;
    }
  }
  return true;
}

export function runDiff(seed0, count, opts = {}) {
  const rng = new Random(seed0);
  const tool = opts.tool || C_TOOL;
  const nodeBin = opts.nodeBin || process.execPath;
  const refJs = opts.refJs || REF_JS;
  let failures = 0;
  for (let t = 0; t < count; t++) {
    const rows = rng.randint(4, 9);
    const cols = rng.randint(4, 9);
    const [mines, board] = genBoard(rng, rows, cols);
    const text = `${rows} ${cols} ${mines}\n` + board.join("\n") + "\n";
    if (!compareBoards(text, tool, nodeBin, refJs, t)) failures += 1;
  }
  return { boards: count, failures };
}

function main() {
  const seed0 = process.argv[2] !== undefined ? Number(process.argv[2]) : 1;
  const count = process.argv[3] !== undefined ? Number(process.argv[3]) : 500;
  const { boards, failures } = runDiff(seed0, count);
  console.log(`done: ${boards} boards, ${failures} mismatches`);
  return failures === 0 ? 0 : 1;
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  process.exit(main());
}
