// analyze-ref.js - reference probabilities via the Node solver.
//
// Zero-dependency port of tools/analyze_test.py.  Reads the same board format
// as analyze_test.c from stdin and prints the identical per-cell layout so the
// two can be diffed.
//
//   line 1: rows cols mines
//   lines 2..: one row per line; '0'-'8' revealed number, '*' revealed mine,
//              'F' flag, '?' question-mark, anything else hidden.
//
// analyze-diff.js compares the C engine's output against this reference
// numerically, so the exact %.12g formatting only needs to be parseable, not
// byte-identical.

import path from "node:path";
import { fileURLToPath } from "node:url";
import { Board, buildConstraints, frontierProbabilities } from "../core/solver.js";

function fmtG(x) {
  if (Number.isNaN(x)) return "nan";
  if (Object.is(x, -0)) x = 0;
  if (x === 0) return "0";
  const s = x.toPrecision(12);
  if (s.includes("e")) return s;
  // strip trailing zeros after the decimal point (printf %g style)
  return s.replace(/(\.\d*?)0+$/, "$1").replace(/\.$/, "");
}

export function analyzeBoard(lines) {
  if (lines[0].charCodeAt(0) === 0xfeff) lines[0] = lines[0].slice(1); // strip BOM
  const [rows, cols, mines] = lines[0].split(/\s+/).map((x) => Number(x));
  const board = new Board(rows, lines.slice(1), mines);

  const cons = buildConstraints(board);
  const { probs, nonfrontierP } = frontierProbabilities(board, cons);

  const n = rows * cols;
  const hidden = [];
  for (let i = 0; i < n; i++) if (board.hidden(i)) hidden.push(i);

  let nf = nonfrontierP;
  let probMap = probs;
  if (probMap.size === 0 && nf === null && hidden.length) {
    // C engine's uniform fallback: nothing is solvable, every hidden cell is
    // a fair guess (the solver would just guess at random)
    nf = mines / hidden.length;
    probMap = new Map();
  }

  const out = [];
  for (const i of hidden) {
    let p = probMap.get(i);
    if (p === undefined) p = nf !== null ? nf : 0.0;
    if (p === undefined || p === null) p = 0.0;
    // reveals: flood from i over hidden un-flagged cells, stopping at numbers
    const revealedOpen = new Set();
    const stack = [i];
    while (stack.length) {
      const x = stack.pop();
      if (revealedOpen.has(x)) continue;
      revealedOpen.add(x);
      if (board.num[x] === 0) {
        // zero cell opens neighbours
        const [r, c] = board.rc(x);
        for (const nb of board.neighbors(r, c)) {
          if (board.hidden(nb) && !revealedOpen.has(nb)) stack.push(nb);
        }
      }
    }
    const [r, c] = board.rc(i);
    out.push(
      `${i} ${r} ${c} ${fmtG(p)} ${fmtG(1.0 - p)} ` +
        `${revealedOpen.size} ${probMap.has(i) ? 1 : 0}`,
    );
  }
  const nfShow = nf !== null ? nf : 0.0;
  out.push(`# ${hidden.length} ${hidden.length - probMap.size} ${fmtG(nfShow)}`);
  return out;
}

export function runFromStdin() {
  return new Promise((resolve, reject) => {
    let data = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => {
      data += chunk;
    });
    process.stdin.on("end", () => {
      try {
        if (data.charCodeAt(0) === 0xfeff) data = data.slice(1); // strip BOM
        const lines = data.split(/\r?\n/).filter((l) => l.trim() !== "");
        for (const line of analyzeBoard(lines)) console.log(line);
        resolve(0);
      } catch (e) {
        console.error("analyze-ref: " + e.message);
        reject(e);
      }
    });
    process.stdin.on("error", reject);
  });
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  runFromStdin().then(
    () => process.exit(0),
    () => process.exit(1),
  );
}
