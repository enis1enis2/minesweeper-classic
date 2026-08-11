import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { SimBoard } from "../core/sim-engine.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const goldens = JSON.parse(
  fs.readFileSync(path.join(here, "fixtures", "golden-boards.json"), "utf8")
);

test("SimBoard matches Python golden boards", () => {
  assert.ok(goldens.length >= 30, "golden fixture present");
  let checked = 0;
  for (const [diff, seed, [r, c], boardLines, state] of goldens) {
    const b = new SimBoard();
    b.new(diff, BigInt(seed));
    b.click(r, c);
    assert.deepEqual(b.board(), boardLines, `board diff=${diff} seed=${seed} click=${r},${c}`);
    assert.equal(b.state().board, state.board, `state.board diff=${diff} seed=${seed}`);
    assert.equal(b.state().status, state.status, `state.status diff=${diff} seed=${seed}`);
    checked++;
  }
  assert.equal(checked, goldens.length);
});

test("R(-5)==R(5): negative seeds share key", () => {
  const a = new SimBoard();
  const b = new SimBoard();
  a.new("beginner", -5n);
  b.new("beginner", 5n);
  assert.deepEqual(a.board(), b.board());
});
