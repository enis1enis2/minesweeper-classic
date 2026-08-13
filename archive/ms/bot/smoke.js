// smoke.js - quick smoke test: play N games and print results.
//
// Zero-dependency port of minesweeper_bot/ms_smoke.py.  Drives the headless
// SimClient (the verified mirror of the real game); the --port flag is
// accepted for CLI compatibility but ignored.
//
// Usage:
//   node ms/bot/smoke.js [--games N] [--difficulty beginner]

import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { Random } from "../core/mt19937.js";
import { SimClient } from "../core/sim-engine.js";
import { playGame } from "../core/solver.js";

export function run(args) {
  const rng = new Random(1);
  let wins = 0;
  for (let g = 0; g < args.games; g++) {
    const client = new SimClient(null, 1000 + g, true);
    const res = playGame(client, args.difficulty, new Map([["tiebreak", "info"]]), rng);
    wins += res.win ? 1 : 0;
    const status = res.win ? "WIN " : "LOSS";
    console.log(
      `${status} time=${res.time}s moves=${res.moves} chords=${res.chords} ` +
        `flags=${res.flags} guesses=${res.guesses}`,
    );
  }
  console.log(
    `\n${args.difficulty}: ${wins}/${args.games} won (${((wins / args.games) * 100).toFixed(1)}%)`,
  );
  return 0;
}

function usage() {
  console.log(
    "usage: node ms/bot/smoke.js [--port PORT] [--games N] [--difficulty beginner|intermediate|expert]",
  );
}

async function main() {
  const { values, positionals } = parseArgs({
    options: {
      port: { type: "string", default: "31350" },
      games: { type: "string", default: "10" },
      difficulty: { type: "string", default: "beginner" },
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
