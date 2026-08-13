// verify-parity.js - prove the simulated board matches the real game.
//
// Zero-dependency port of server/verify_parity.py.  For a set of
// (difficulty, seed, first-click) triples this tool:
//   1. starts minesweeper-x64.exe --listen <port>,
//   2. applies the seed as a persistent Normal seed and plays the first click
//      through the scripting CLI,
//   3. builds the same board in SimBoard and clicks the same cell,
//   4. compares the two `board` dumps cell-by-cell (and `opened`/`over`).
//
// A mismatch means the sim's xorshift64 / place_mines / reveal logic drifted
// from the C code.  Run from the ms/ directory or repo root (Windows only;
// needs the compiled exe).

import { spawn } from "node:child_process";
import net from "node:net";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { Random } from "../core/mt19937.js";
import { SimBoard } from "../core/sim-engine.js";
import { MSClient } from "../core/client.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "..", "..");
const DEFAULT_EXE = path.join(REPO, "build", "minesweeper-x64.exe");

const DIFFS = ["beginner", "intermediate", "expert"];
// (rows, cols) per difficulty, used to pick first-click positions.
const SIZES = { beginner: [8, 8], intermediate: [16, 16], expert: [16, 30] };
const FIRST_CLICKS = ["center", "corner", "edge"];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function firstCell(diff, pos) {
  const [rows, cols] = SIZES[diff];
  if (pos === "center") return [Math.floor(rows / 2), Math.floor(cols / 2)];
  if (pos === "corner") return [0, 0];
  return [Math.floor(rows / 2), 0];
}

function findFreePort(base) {
  return new Promise((resolve, reject) => {
    const tryBind = (p) => {
      const srv = net.createServer();
      srv.once("error", () => {
        if (p < base + 200) tryBind(p + 1);
        else reject(new Error("no free port near " + base));
      });
      srv.listen(p, "127.0.0.1", () => {
        const port = srv.address().port;
        srv.close(() => resolve(port));
      });
    };
    tryBind(base);
  });
}

async function waitPort(port, exeProc, timeout = 15000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeout) {
    if (exeProc.exitCode !== null) {
      throw new Error("exe exited early with code " + exeProc.exitCode);
    }
    const ok = await new Promise((resolve) => {
      const s = net.connect(port, "127.0.0.1");
      s.once("connect", () => {
        s.destroy();
        resolve(true);
      });
      s.once("error", () => resolve(false));
      s.setTimeout(1000, () => {
        s.destroy();
        resolve(false);
      });
    });
    if (ok) return;
    await sleep(100);
  }
  throw new Error("game did not open port " + port);
}

export async function run(args) {
  if (!args.exe || !exists(args.exe)) {
    console.log("exe not found: " + args.exe + " (build it first)");
    return 2;
  }
  const port = args.port || (await findFreePort(31350));

  const exe = spawn(args.exe, ["--listen", String(port)], {
    stdio: "ignore",
  });
  try {
    await waitPort(port, exe);
    const client = new MSClient(port);
    if (!(await client.ping())) {
      console.log("game did not answer ping");
      return 2;
    }

    const rng = new Random(20260209);
    let fails = 0;
    let total = 0;
    const combos = [];
    for (const diff of DIFFS) {
      const positions = FIRST_CLICKS.slice();
      if (args.fast && diff === "expert") positions.length = 1; // center only
      for (const pos of positions) {
        for (let i = 0; i < args.seeds; i++) {
          combos.push([diff, pos, rng.randrange(0n, 1n << 63n)]);
        }
      }
    }

    for (const [diff, pos, seed] of combos) {
      total += 1;
      const [r0, c0] = firstCell(diff, pos);
      const attempt = async () => {
        await client.seedDiff(diff, seed);
        await client.new(diff);
        await client.click(r0, c0);
        const liveState = await client.state();
        const liveBoard = await client.board();

        const sim = new SimBoard();
        sim.new(diff, seed);
        sim.click(r0, c0);
        const simBoard = sim.board();
        const simState = sim.state();

        const mismatches = [];
        if (simBoard.join("\n") !== liveBoard.join("\n")) mismatches.push("board differs");
        for (const key of ["opened", "over", "started"]) {
          if (simState[key] !== liveState[key]) {
            mismatches.push(`${key} ${simState[key]}!=${liveState[key]}`);
          }
        }
        return { mismatches, liveBoard, simBoard };
      };

      let result = await attempt();
      if (result.mismatches.length) {
        // The live game occasionally drops a scripting click (opened=0,
        // started=0) while the sim proceeded; re-verify the same combo once
        // before treating a difference as a real parity failure.
        result = await attempt();
      }
      if (result.mismatches.length) {
        fails += 1;
        console.log(
          `  MISMATCH ${diff} seed=${seed} first=(${r0},${c0}) pos=${pos}: ${result.mismatches.join("; ")}`,
        );
        for (let i = 0; i < result.liveBoard.length && i < result.simBoard.length; i++) {
          if (result.liveBoard[i] !== result.simBoard[i]) {
            console.log(`    live row ${i}: ${result.liveBoard[i]}`);
            console.log(`    sim  row ${i}: ${result.simBoard[i]}`);
            break;
          }
        }
      }
    }

    console.log(
      `parity: ${total - fails}/${total} boards identical (${fails} mismatch${fails === 1 ? "" : "es"})`,
    );
    await client.close();
    return fails === 0 ? 0 : 1;
  } finally {
    try {
      exe.kill();
    } catch {
      // ignore
    }
  }
}

function exists(p) {
  try {
    return fs.statSync(p).isFile();
  } catch {
    return false;
  }
}

function usage() {
  console.log(
    "usage: node ms/cli/verify-parity.js [--exe ..\\build\\minesweeper-x64.exe] " +
      "[--port 31350] [--seeds 8] [--fast]",
  );
}

async function main() {
  const { values, positionals } = parseArgs({
    options: {
      exe: { type: "string", default: DEFAULT_EXE },
      port: { type: "string", default: "0" },
      seeds: { type: "string", default: "8" },
      fast: { type: "boolean", default: false },
      help: { type: "boolean", default: false },
    },
    allowPositionals: true,
  });

  if (values.help || positionals.length) {
    usage();
    return values.help ? 0 : 2;
  }

  return run({
    exe: values.exe,
    port: Number(values.port),
    seeds: Number(values.seeds),
    fast: values.fast,
  });
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  process.exit(await main());
}

export { main };
