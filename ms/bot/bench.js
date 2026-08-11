// bench.js - benchmark harness: plays many games per difficulty/strategy.
//
// Zero-dependency port of minesweeper_bot/ms_bench.py.  Unlike the Python
// original (which needed a running `minesweeper-x64.exe --listen <port>`), the
// Node port drives the headless SimClient against the sim engine, which is the
// byte-identical mirror of the real game (proved by ms-verify-parity).  Board
// seeding and the decision RNG are unchanged, so win rates match what a live
// --listen instance would produce for the same seeds.
//
// Usage:
//   node ms/bot/bench.js --difficulty beginner --games 100
//   node ms/bot/bench.js --all --games 200
//   node ms/bot/bench.js --sweep            # sweep strategy knobs, print table
//
// The --port flag is accepted for CLI compatibility with the Python tooling
// but ignored: no live game instance is required.

import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { Random } from "../core/mt19937.js";
import { SimClient } from "../core/sim-engine.js";
import { playGame } from "../core/solver.js";

const perfNow = () => performance.now();

export function benchmark(difficulty, games, strategy, seedBase = 0, verbose = false) {
  const strat = new Map(strategy);
  strat.set("refresh", false); // bots don't need repaints
  const results = [];
  const start = performance.now();
  for (let g = 0; g < games; g++) {
    // deterministic per-game seed so different strategies see the same
    // boards (fair comparison)
    const rng2 = new Random(seedBase * 1000000 + g);
    const seed = rng2.randrange(0, 2 ** 31);
    const rng = new Random(seed); // decision RNG derived from the board seed
    const client = new SimClient(null, seed, true);
    const g0 = perfNow();
    const res = playGame(client, difficulty, strat, rng);
    res.wall = perfNow() - g0;
    res.game = g;
    results.push(res);
    if (verbose && (g + 1) % 25 === 0) {
      console.error(`  ${difficulty}: ${g + 1}/${games} games`);
    }
  }
  const elapsed = (performance.now() - start) / 1000;

  const wins = results.filter((r) => r.win);
  const winRate = (wins.length / Math.max(1, results.length)) * 100.0;
  const times = wins.map((r) => r.wall);
  return {
    difficulty,
    games,
    wins: wins.length,
    win_rate: winRate,
    avg_time: times.length ? sum(times) / times.length : null,
    fastest: times.length ? Math.min(...times) : null,
    slowest: times.length ? Math.max(...times) : null,
    avg_moves: wins.length ? sum(wins.map((r) => r.moves)) / wins.length : null,
    avg_chords: wins.length ? sum(wins.map((r) => r.chords)) / wins.length : null,
    strategy: Object.fromEntries(strat),
    wall_s: elapsed,
  };
}

function sum(arr) {
  return arr.reduce((a, b) => a + b, 0);
}

export const STRATEGIES = [
  { name: "minprob-random", tiebreak: "random" },
  { name: "minprob-info", tiebreak: "info" },
];

export function run(args) {
  const diffs = args.all ? ["beginner", "intermediate", "expert"] : [args.difficulty];
  const seedBase = { beginner: 0, intermediate: 1, expert: 2 };

  let variants;
  if (args.sweep) {
    variants = [];
    for (const tie of ["random", "info"]) {
      for (const first of ["center", "corner"]) {
        for (const chord of [true, false]) {
          variants.push({
            name: `tie=${tie},first=${first},chord=${chord}`,
            tiebreak: tie,
            first,
            use_chord: chord,
          });
        }
      }
    }
  } else {
    variants = STRATEGIES;
  }

  if (args.port !== 31350) {
    console.error(
      `note: running headless against the sim engine (verified mirror of the game); --port ignored`,
    );
  }

  for (const d of diffs) {
    console.log(`\n===== ${d} =====`);
    const rows = [];
    for (const v of variants) {
      const strat = new Map();
      for (const [k, val] of Object.entries(v)) if (k !== "name") strat.set(k, val);
      const r = benchmark(d, args.games, strat, seedBase[d], args.verbose);
      r.name = v.name;
      rows.push(r);
      const wr = r.win_rate;
      const ft = r.fastest !== null ? r.fastest.toFixed(2) : "-";
      const at = r.avg_time !== null ? r.avg_time.toFixed(2) : "-";
      const avgMoves = r.avg_moves || 0;
      console.log(
        `  ${String(v.name).padEnd(32)} win=${wr.toFixed(2).padStart(6)}%  ` +
          `fastest=${String(ft).padStart(6)}s  avg=${String(at).padStart(6)}s  ` +
          `avg_moves=${avgMoves.toFixed(1).padStart(6)}`,
      );
    }
    const best = rows.reduce(
      (a, b) =>
        b.win_rate > a.win_rate || (b.win_rate === a.win_rate && -(b.avg_time || 0) > -(a.avg_time || 0))
          ? b
          : a,
      rows[0],
    );
    console.log(`  -> best: ${best.name} win=${best.win_rate.toFixed(2)}%`);
  }
  return 0;
}

function usage() {
  console.log(
    "usage: node ms/bot/bench.js [--difficulty beginner|intermediate|expert] " +
      "[--games N] [--port PORT] [--all] [--sweep] [--verbose]",
  );
}

async function main() {
  const { values, positionals } = parseArgs({
    options: {
      difficulty: { type: "string", default: "beginner" },
      games: { type: "string", default: "100" },
      port: { type: "string", default: "31350" },
      all: { type: "boolean", default: false },
      sweep: { type: "boolean", default: false },
      verbose: { type: "boolean", default: false },
      help: { type: "boolean", default: false },
    },
    allowPositionals: true,
  });

  if (values.help || positionals.length) {
    usage();
    return values.help ? 0 : 2;
  }

  return run({
    difficulty: values.difficulty,
    games: Number(values.games),
    port: Number(values.port),
    all: values.all,
    sweep: values.sweep,
    verbose: values.verbose,
  });
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  process.exit(await main());
}

export { main };
