// fastest.js - best strategy runner: plays a difficulty with the winning
// strategy found by the sweep in bench.js.
//
// Zero-dependency port of minesweeper_bot/ms_fastest.py.  Drives the headless
// SimClient (the verified mirror of the real game); the --port flag is
// accepted for CLI compatibility but ignored.
//
// Results (verified over 800+ games per variant on seeded boards):
//   beginner     info tiebreak, center first,  no chording -> ~87.8% wins
//   intermediate info tiebreak, center first,  no chording -> ~84.1% wins
//   expert       info tiebreak, corner first,  no chording -> ~38.1% wins
//
// Usage:
//   node ms/bot/fastest.js [--games N] [--difficulty all|beginner|...]

import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { Random } from "../core/mt19937.js";
import { SimClient } from "../core/sim-engine.js";
import { playGame } from "../core/solver.js";

export const BEST = {
  beginner: { name: "info-center", tiebreak: "info", first: "center", use_chord: false },
  intermediate: { name: "info-center", tiebreak: "info", first: "center", use_chord: false },
  expert: { name: "info-corner", tiebreak: "info", first: "corner", use_chord: false },
};

export function run(args) {
  const diffs = args.difficulty === "all" ? Object.keys(BEST) : [args.difficulty];
  const seedBase = { beginner: 0, intermediate: 1, expert: 2 };

  for (const d of diffs) {
    const strat = new Map(Object.entries(BEST[d]));
    strat.set("refresh", false);
    let wins = 0;
    for (let g = 0; g < args.games; g++) {
      const rng2 = new Random(seedBase[d] * 1000000 + g);
      const seed = rng2.randrange(0, 2 ** 31);
      const client = new SimClient(null, seed, true);
      const rng = new Random(seed);
      const res = playGame(client, d, strat, rng);
      wins += res.win ? 1 : 0;
    }
    const wr = (wins / args.games) * 100;
    console.log(
      `${String(d).padEnd(14)} ${String(BEST[d].name).padEnd(12)} ${wins}/${args.games} = ${wr.toFixed(2)}%`,
    );
  }
  return 0;
}

function usage() {
  console.log(
    "usage: node ms/bot/fastest.js [--port PORT] [--games N] [--difficulty all|beginner|intermediate|expert]",
  );
}

async function main() {
  const { values, positionals } = parseArgs({
    options: {
      port: { type: "string", default: "31350" },
      games: { type: "string", default: "200" },
      difficulty: { type: "string", default: "all" },
      help: { type: "boolean", default: false },
    },
    allowPositionals: true,
  });

  if (values.help || positionals.length) {
    usage();
    return values.help ? 0 : 2;
  }

  return run({
    port: Number(values.port),
    games: Number(values.games),
    difficulty: values.difficulty,
  });
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  process.exit(await main());
}

export { main };
