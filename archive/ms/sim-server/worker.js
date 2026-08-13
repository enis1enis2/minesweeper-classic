// worker.js - worker_threads entry that plays one simulated game on demand.
//
// The main thread hands it a difficulty/seed plus either a serialized RNG
// snapshot (broadcast games: the decision RNG is the producer's shared stream,
// so the worker returns the advanced snapshot for the main thread to adopt)
// or a fresh decision seed (requested games: Random(seed ^ run<<32), no state
// to carry).  Returns the DB row dict minus `requester`, which the main
// thread attaches.

import { parentPort } from "node:worker_threads";
import { SimBoard, SimClient } from "../core/sim-engine.js";
import { playGame, Random } from "../core/solver.js";

// server/ms_server.py SOLVER_STRATEGY verbatim.
const STRATEGY = new Map([
  ["tiebreak", "info"],
  ["first", "center"],
  ["use_chord", true],
  ["refresh", false],
]);

parentPort.on("message", (task) => {
  try {
    const board = new SimBoard();
    board.new(task.diff, BigInt(task.seed));
    const client = new SimClient(board, BigInt(task.seed));
    const rng = task.rngState
      ? Random.fromState(task.rngState)
      : new Random(BigInt(task.decisionSeed));
    const t0 = performance.now();
    const res = playGame(client, task.diff, STRATEGY, rng);
    const g = {
      ts: Math.floor(Date.now() / 1000),
      difficulty: task.diff,
      seed: task.seed,
      won: res.win,
      moves: res.moves,
      time_ms: Math.round(res.time * 1000),
      guesses: res.guesses,
      chords: res.chords,
      flags: res.flags,
      deduce_batches: res.deduce_batches,
      frontier: res.frontier,
      wall_ms: Math.floor(performance.now() - t0),
    };
    parentPort.postMessage({
      id: task.id,
      g,
      rngState: task.rngState ? rng.snapshot() : undefined,
    });
  } catch (err) {
    parentPort.postMessage({
      id: task.id,
      error: String(err && err.stack ? err.stack : err),
    });
  }
});
