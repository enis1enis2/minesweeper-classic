// profile.js - profile one expert game to find the hotspot.
//
// Zero-dependency port of minesweeper_bot/ms_profile.py.  Plays a single
// expert game (seed 4242, info tiebreak) and prints the result.  The Python
// original used cProfile; the Node equivalent is V8's CPU profiler, which is
// enabled from the command line (no code changes needed):
//
//   node --cpu-prof --cpu-prof-dir=prof ms/bot/profile.js
//
// That writes a prof/*.cpuprofile you can open in DevTools / Speedscope.
// Wall-clock time for this one game is printed regardless.

import path from "node:path";
import { fileURLToPath } from "node:url";
import { Random } from "../core/mt19937.js";
import { SimClient } from "../core/sim-engine.js";
import { playGame } from "../core/solver.js";

export function run() {
  const client = new SimClient(null, 4242, true);
  const rng = new Random(1);
  const t0 = performance.now();
  const res = playGame(client, "expert", new Map([["tiebreak", "info"]]), rng);
  const wall = (performance.now() - t0) / 1000;
  console.log(JSON.stringify(res));
  console.log(`wall: ${wall.toFixed(3)}s`);
  if (!process.argv.some((a) => a.startsWith("--cpu-prof"))) {
    console.log("tip: run with `node --cpu-prof --cpu-prof-dir=prof ms/bot/profile.js` for a V8 CPU profile");
  }
  return 0;
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  process.exit(run());
}
