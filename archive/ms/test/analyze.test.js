import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { analyzeBoard } from "../analyze/analyze-ref.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.join(here, "..", "..");

const GOLDEN = [
  "1 0 1 0.5 0.5 15 1",
  "3 0 3 0.5 0.5 15 1",
  "6 1 1 0.5 0.5 15 1",
  "7 1 2 0 1 15 1",
  "8 1 3 0.5 0.5 15 1",
  "10 2 0 0.5 0.5 15 1",
  "11 2 1 0.5 0.5 15 1",
  "12 2 2 0 1 15 0",
  "13 2 3 0.5 0.5 15 1",
  "14 2 4 0.5 0.5 15 1",
  "16 3 1 0.5 0.5 15 1",
  "17 3 2 0 1 15 1",
  "18 3 3 0.5 0.5 15 1",
  "21 4 1 0.5 0.5 15 1",
  "23 4 3 0.5 0.5 15 1",
  "# 15 1 0",
];

const FIXTURE = ["5 5 4", "1.2.1", "2...2", ".....", "2...2", "1.2.1"];

test("analyze-ref matches the C engine byte-for-byte on the 5x5 fixture", () => {
  assert.deepEqual(analyzeBoard(FIXTURE), GOLDEN);
});

test("analyze-ref output is structurally sane", () => {
  const out = analyzeBoard(FIXTURE);
  const cells = out.filter((l) => !l.startsWith("#"));
  assert.equal(cells.length, 15, "15 hidden cells");
  assert.equal(out[out.length - 1], "# 15 1 0", "hidden front nonfrontier");
  for (const l of cells) {
    const f = l.split(/\s+/).map(Number);
    assert.equal(f.length, 7);
    assert.ok(f[3] >= 0 && f[3] <= 1, "p in [0,1]");
    assert.ok(Math.abs(f[3] + f[4] - 1) < 1e-9, "p + 1-p = 1");
  }
});

test("analyze-ref uniform fallback on an unsolvable board", () => {
  // No revealed numbers touch any hidden cell, so nothing is solvable; the
  // C fallback assigns every hidden cell nf = mines/hidden as a fair guess.
  const out = analyzeBoard(["1 2 1", ".."]);
  assert.deepEqual(out, [
    "0 0 0 0.5 0.5 2 0",
    "1 0 1 0.5 0.5 2 0",
    "# 2 2 0.5",
  ]);
});

test("analyze-ref strips a BOM-prefixed header", () => {
  assert.deepEqual(analyzeBoard(["\ufeff" + FIXTURE[0], ...FIXTURE.slice(1)]), GOLDEN);
});

test("analyze-diff matches the C engine on a small batch", { skip: !fs.existsSync(path.join(root, "build", "analyze_test.exe")) }, async () => {
  const { runDiff } = await import("../analyze/analyze-diff.js");
  const res = runDiff(1, 15);
  assert.equal(res.failures, 0, "all boards agree with the C engine");
  assert.equal(res.boards, 15);
});
