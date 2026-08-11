import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { SimBoard } from "../core/sim-engine.js";
import {
  Board,
  buildConstraints,
  frontierProbabilities,
  Random,
} from "../core/solver.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtures = path.join(here, "fixtures");

test("frontier probabilities match Python on a shared mid-game board", () => {
  const [lines, expectedProbs, expectedNfp] = JSON.parse(
    fs.readFileSync(path.join(fixtures, "golden-probs.json"), "utf8")
  );
  const sim = new SimBoard();
  sim.new("expert", 12345n);
  for (let i = 0; i < 10; i++) sim.click(14, 14);
  assert.deepEqual(sim.board(), lines, "board reproduction matches Python fixture");
  const b = new Board(sim.rows, sim.board(), sim.mines);
  const cons = buildConstraints(b);
  const { probs, nonfrontierP } = frontierProbabilities(b, cons);
  // Python emits frozenset-ordered cells (implementation detail); sort both.
  const jsProbs = [...probs.entries()]
    .map(([c, p]) => [String(c), p])
    .sort((a, b) => Number(a[0]) - Number(b[0]));
  const expectedSorted = [...expectedProbs].sort(
    (a, b) => Number(a[0]) - Number(b[0])
  );
  assert.deepEqual(jsProbs, expectedSorted, "frontier probabilities");
  assert.equal(nonfrontierP, expectedNfp, "nonfrontier probability");
});

test("playGame runs end to end with deterministic seed", async () => {
  // Smoke: deterministic (not golden) — proves wiring, not move equality.
  const { playGame } = await import("../core/solver.js");
  const { SimClient } = await import("../core/sim-engine.js");
  const rng = new Random(1000);
  const c = new SimClient(null, 9000n, true);
  const res = playGame(c, "expert", new Map([
    ["first", "center"],
    ["tiebreak", "info"],
    ["use_chord", true],
    ["refresh", true],
  ]), rng);
  assert.equal(typeof res.win, "boolean");
  assert.equal(typeof res.time, "number");
  assert.ok(res.moves > 0);
});
