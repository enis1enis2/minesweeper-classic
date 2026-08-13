import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Mt19937, Random } from "../core/mt19937.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const goldens = JSON.parse(
  fs.readFileSync(path.join(here, "fixtures", "golden-rng.json"), "utf8")
);

test("CPython random.Random(5489) first getrandbits(32) matches", () => {
  const m = new Mt19937(5489);
  assert.equal(m.getrandbits(32), 3382763572n);
});

test("JS random streams match Python random.Random (int seeds)", () => {
  for (const [seed, rnd10, gb64] of goldens) {
    const r = new Random(BigInt(seed));
    for (const expected of rnd10) {
      assert.equal(r.random(), expected, `seed=${seed}`);
    }
  }
});

test("JS getrandbits(64) matches Python for each seed stream", () => {
  for (const [seed, , gb64] of goldens) {
    const r = new Random(BigInt(seed));
    for (const expected of gb64) {
      assert.equal(r.getrandbits(64).toString(), expected, `seed=${seed}`);
    }
  }
});

test("randrange / randint / choice / shuffle match Python semantics", () => {
  const r = new Random(7);
  assert.equal(r.randrange(0, 10), 5);
  assert.equal(r.randint(1, 6), 2);
  assert.equal(r.randrange(2n ** 60n, 2n ** 60n + 5n, 2n), 1152921504606846978n);
  assert.equal(r.choice(["a", "b", "c"]), "c");
  const arr = [0, 1, 2, 3, 4];
  r.shuffle(arr);
  assert.deepEqual(arr, [1, 3, 2, 4, 0]);
});
