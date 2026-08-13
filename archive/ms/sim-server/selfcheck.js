// selfcheck.js - no-network sanity pass (port of ms_server.py selfcheck()).
//
// Runs 20 simulated games per difficulty through the solver with a fixed RNG
// and reports win counts / average moves.  Exit 0 = PASS, 1 = FAIL.

import { SimBoard, SimClient } from "../core/sim-engine.js";
import { playGame, Random } from "../core/solver.js";
import { DIFFS, SOLVER_STRATEGY } from "./config.js";

export async function selfcheck() {
  const rng = new Random(42);
  let ok = true;
  for (const diff of DIFFS) {
    let wins = 0;
    let games = 0;
    let movesSum = 0;
    for (let i = 0; i < 20; i++) {
      const seed = rng.randrange(0n, 1n << 63n);
      const board = new SimBoard();
      board.new(diff, seed);
      const client = new SimClient(board, seed);
      const res = playGame(client, diff, SOLVER_STRATEGY, rng);
      games += 1;
      if (res.win) wins += 1;
      movesSum += res.moves;
      if (res.moves < 1) {
        console.log(`  FAIL ${diff}: no moves`);
        ok = false;
      }
      const b = client.board();
      if (b.length !== board.rows || b.some((r) => r.length !== board.cols)) {
        console.log(`  FAIL ${diff}: board size mismatch`);
        ok = false;
      }
    }
    const tag = wins > 0 ? "  OK" : "";
    console.log(
      `  ${diff.padEnd(12)} games=${games} wins=${wins} avg_moves=${(
        movesSum / Math.max(1, games)
      ).toFixed(1)}${tag}`
    );
    if (wins === 0) ok = false;
  }
  console.log("selfcheck:", ok ? "PASS" : "FAIL");
  return ok ? 0 : 1;
}
