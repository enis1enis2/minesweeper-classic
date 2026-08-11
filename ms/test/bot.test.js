import { test } from "node:test";
import assert from "node:assert/strict";
import { benchmark } from "../bot/bench.js";

test("benchmark is deterministic for the same seed base", () => {
  const strat = new Map([
    ["tiebreak", "info"],
    ["first", "center"],
    ["use_chord", false],
    ["refresh", false],
  ]);
  const a = benchmark("beginner", 40, strat, 0);
  const b = benchmark("beginner", 40, strat, 0);
  assert.equal(a.wins, b.wins, "same seeds -> same win count");
  assert.equal(a.avg_moves, b.avg_moves);
});

test("benchmark reports sane aggregates", () => {
  const strat = new Map([
    ["tiebreak", "info"],
    ["first", "center"],
    ["use_chord", false],
    ["refresh", false],
  ]);
  const r = benchmark("beginner", 50, strat, 0);
  assert.equal(r.games, 50);
  assert.ok(r.wins <= r.games);
  assert.ok(r.win_rate >= 0 && r.win_rate <= 100);
  assert.ok(r.avg_moves > 0);
  assert.ok(r.avg_time !== null);
  assert.ok(r.fastest > 0);
});

test("documented win rates hold within sampling error", () => {
  // Python bot docs (verified over 800+ games):
  //   beginner ~87.8%, intermediate ~84.1%, expert ~38.1% (info/center & info/corner).
  // We assert generous bounds; a regression in the solver would drop these hard.
  const cases = [
    ["beginner", 0, "center", 0.70],
    ["intermediate", 1, "center", 0.66],
    ["expert", 2, "corner", 0.26],
  ];
  for (const [diff, base, first, floor] of cases) {
    const strat = new Map([
      ["tiebreak", "info"],
      ["first", first],
      ["use_chord", false],
      ["refresh", false],
    ]);
    const r = benchmark(diff, 400, strat, base);
    assert.ok(
      r.win_rate >= floor * 100,
      `${diff} win_rate ${r.win_rate.toFixed(2)}% below floor ${floor * 100}%`,
    );
  }
});
