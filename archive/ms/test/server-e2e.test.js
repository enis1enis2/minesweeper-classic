// server-e2e.test.js - end-to-end wire-protocol test for the Node sim server.
// Port of server/selfcheck.py run against the Node server: spawns
// sim-server/server.js on a free port, exercises the broadcast stream,
// metrics, leaderboard, solver auth gate (wrong user / wrong password /
// handshake / lockout) and reqseed/reqbatch, then verifies SQLite contents.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import crypto from "node:crypto";
import { spawn } from "node:child_process";
import {
  LineReader,
  spawnServer,
  drainBroadcast,
  authHandshake,
  expect,
  expectAny,
  collectRequests,
  stopProc,
  MS_DIR,
} from "./helpers/e2e.js";

const SOLVER_USER = "testuser";
const SOLVER_PASS = "test-secret-123";

function readRow(db, sql) {
  const stmt = db.prepare(sql);
  return stmt.get();
}

function readAll(db, sql) {
  const stmt = db.prepare(sql);
  return stmt.all();
}

test("sim server end-to-end wire protocol", { timeout: 120000 }, async () => {
  try {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "ms-e2e-"));
    const db = path.join(tmp, "sim.db");

    // ---------- server without solver creds ----------
    const { proc, sock } = await spawnServer(0, db);
    try {
      const reader = new LineReader(sock);
      const [seed, outcome] = await drainBroadcast(reader);
      assert.equal(seed[1], outcome[1]);
      assert.equal(seed[2], outcome[2]);

      sock.write("metric start diff=beginner seed=1 seeded=1 t=1\n");
      sock.write(
        "metric win diff=beginner seed=1 seeded=1 time=42 clicks=10 latency=123 t=2\n"
      );
      await new Promise((r) => setTimeout(r, 500));

      // solver disabled -> denied
      sock.write("reqseed beginner 12345\n");
      const denied = await expect(reader, "reqdenied", "reqdenied");
      assert.equal(denied[0], "reqdenied");

      // leaderboard: submit + improve + slower (no rank change)
      sock.write("lbscore Player1 beginner 45000\n");
      let parts = await expect(reader, "lbstored", "lbstored");
      assert.deepEqual(parts.slice(1, 5), ["1", "beginner", "Player1", "45000"]);
      sock.write("lbscore Player1 beginner 40000\n");
      parts = await expect(reader, "lbstored", "lbstored");
      assert.deepEqual(parts.slice(1, 5), ["1", "beginner", "Player1", "40000"]);
      sock.write("lbscore Player1 beginner 42000\n");
      await expect(reader, "lbnotop", "lbnotop");
      sock.write("lbscore Player2 intermediate 120000\n");
      parts = await expect(reader, "lbstored", "lbstored");
      assert.deepEqual(parts.slice(1, 5), [
        "1",
        "intermediate",
        "Player2",
        "120000",
      ]);

      // invalid name ignored
      sock.write("lbscore 'bad name!' beginner 1000\n");
      sock.write("lbtop 10\n");
      parts = await expect(reader, "lbtop", "lbtop header");
      assert.equal(Number(parts[1]), 2);
      const rows = [];
      const dl = Date.now() + 3000;
      while (rows.length < Number(parts[1]) && Date.now() < dl) {
        const line = await Promise.race([
          reader.readLine().catch((e) => "__CLOSED__:" + e.message),
          new Promise((r) => setTimeout(() => r("__TIMEOUT__"), 1500)),
        ]);
        if (line === "__TIMEOUT__") continue;
        const p = line.split(/\s+/);
        if (p[0] === "lbentry") rows.push(p.slice(1, 6).join(" "));
      }
      assert.equal(rows.length, Number(parts[1]));
      await expect(reader, "lbdone", "lbdone");
      assert.ok(rows.some((r) => r.startsWith("1 beginner Player1 40000")));
      assert.ok(rows.some((r) => r.startsWith("1 intermediate Player2 120000")));
      assert.ok(!rows.some((r) => r.includes("bad name")));
    } finally {
      sock.destroy();
      await stopProc(proc);
    }

    // ---------- server WITH solver creds ----------
    const { proc: proc2, sock: sock2 } = await spawnServer(0, db, [
      "--solver-user",
      SOLVER_USER,
      "--solver-pass",
      SOLVER_PASS,
    ]);
    try {
      const reader = new LineReader(sock2);
      await drainBroadcast(reader);

      // unauthenticated requests denied
      sock2.write("reqseed beginner 12345\n");
      await expect(reader, "reqdenied", "reqdenied");
      sock2.write("reqbatch beginner 3\n");
      await expect(reader, "reqdenied", "reqdenied");

      // wrong user
      const wrongUser = await authHandshake(sock2, reader, "nobody", SOLVER_PASS);
      assert.equal(wrongUser, false);

      // wrong password (fresh connection)
      const wrongPass = await authHandshake(sock2, reader, SOLVER_USER, "wrong");
      assert.equal(wrongPass, false);

      // correct credentials
      const ok = await authHandshake(sock2, reader, SOLVER_USER, SOLVER_PASS);
      assert.equal(ok, true);

      // reqseed then reqbatch
      sock2.write("reqseed beginner 12345\n");
      await expectAny(reader, ["reqwait", "reqgame"], "reqseed start");
      let done = await expect(reader, "reqdone", "reqdone");
      assert.deepEqual(done.slice(1), ["beginner", "1"]);

      sock2.write("reqbatch beginner 5\n");
      await expectAny(reader, ["reqwait", "reqgame"], "reqbatch start");
      done = await expect(reader, "reqdone", "reqdone");
      assert.deepEqual(done.slice(1), ["beginner", "5"]);

      // lockout: 5 wrong passwords (fresh challenge each) close the connection
      let closed = false;
      try {
        for (let i = 0; i < 5; i++) {
          sock2.write("auth " + SOLVER_USER + "\n");
          const c = await expect(reader, "authchal", "authchal");
          sock2.write("authresp " + digestWrong(c[1], i) + "\n");
          await expect(reader, "autherr", "autherr");
        }
        await reader.readLine(); // should reject: connection closed
      } catch {
        closed = true;
      }
      assert.ok(closed, "connection closed after 5 failures");
    } finally {
      sock2.destroy();
      await stopProc(proc2);
    }

    // ---------- SQLite contents ----------
    {
      const dbc = new DatabaseSync(db, { readBigInts: true });
      try {
        const games = readRow(dbc, "SELECT COUNT(*) AS n FROM sim_games");
        assert.ok(games.n >= 1, "games recorded");

        const req = readRow(
          dbc,
          "SELECT COUNT(*) AS n FROM sim_games WHERE requester IS NOT NULL"
        );
        assert.equal(Number(req.n), 6); // 1 reqseed + 5 reqbatch

        const replay = readRow(
          dbc,
          "SELECT won FROM sim_games WHERE seed = 12345 AND requester IS NOT NULL"
        );
        assert.ok(replay !== undefined, "requested seed replayed");
        assert.ok(
          typeof replay.won === "bigint" || typeof replay.won === "number",
          "won is an integer"
        );

        const lb = readRow(dbc, "SELECT COUNT(*) AS n FROM leaderboard");
        assert.equal(Number(lb.n), 2);

        const lbRows = readAll(
          dbc,
          "SELECT name, difficulty, time_ms FROM leaderboard ORDER BY difficulty, time_ms"
        );
        assert.deepEqual(
          lbRows.map((r) => [r.name, r.difficulty, Number(r.time_ms)]),
          [
            ["Player1", "beginner", 40000],
            ["Player2", "intermediate", 120000],
          ]
        );

        const metrics = readRow(
          dbc,
          "SELECT COUNT(*) AS n FROM client_metrics"
        );
        assert.equal(Number(metrics.n), 2);

        const clients = readRow(dbc, "SELECT COUNT(*) AS n FROM clients");
        assert.ok(Number(clients.n) >= 1, "clients upserted");
      } finally {
        dbc.close();
      }
    }
  } finally {
    // no port reservation to release (OS-assigned)
  }
});

function digestWrong(nonce, i) {
  return crypto
    .createHmac("sha256", "wrong-" + i)
    .update("ms-auth:" + nonce)
    .digest("hex");
}

// Read (and discard) broadcast lines for `ms`, failing if any line starts
// with a disallowed prefix.  Used to assert the server *ignores* malformed
// input exactly like Python (no reply, no state change).
async function expectSilent(reader, forbidden, ms) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    const parts = (await reader.readLine()).split(/\s+/);
    if (parts.length && forbidden.includes(parts[0])) {
      throw new Error(
        "server replied to malformed input with: " + parts.join(" ")
      );
    }
  }
}

test("server rejects non-decimal integers like Python int()", { timeout: 60000 }, async () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "ms-e2e-int-"));
  const db = path.join(tmp, "sim.db");
  const { proc, sock } = await spawnServer(0, db, [
    "--solver-user",
    SOLVER_USER,
    "--solver-pass",
    SOLVER_PASS,
  ]);
  try {
    const reader = new LineReader(sock);
    await drainBroadcast(reader);
    const ok = await authHandshake(sock, reader, SOLVER_USER, SOLVER_PASS);
    assert.equal(ok, true);

    // Python int()/float() never accept these; the server must stay silent.
    sock.write("lbscore Player9 beginner 1e3\n");
    await expectSilent(reader, ["lbstored", "lbdenied"], 700);
    sock.write("lbscore Player9 beginner 0x10\n");
    await expectSilent(reader, ["lbstored", "lbdenied"], 700);
    sock.write("lbtop 1e1\n");
    await expectSilent(reader, ["lbtop"], 700);
    sock.write("reqseed beginner 0x10\n");
    await expectSilent(reader, ["reqgame", "reqdone"], 700);
    sock.write("reqbatch beginner 1e2\n");
    await expectSilent(reader, ["reqgame", "reqdone"], 700);
    sock.write("requntil beginner 0x10\n");
    await expectSilent(reader, ["reqgame", "reqdone"], 700);

    // a valid request still works afterwards (proves the silences above were
    // not a dead connection) and decimal zero is a legal score value.
    sock.write("reqseed beginner 12345\n");
    await expectAny(reader, ["reqwait", "reqgame"], "reqseed start");
    const done = await expect(reader, "reqdone", "reqdone");
    assert.deepEqual(done.slice(1), ["beginner", "1"]);
    sock.write("lbscore Player9 beginner 0\n");
    await expect(reader, "lbstored", "lbstored");
  } finally {
    sock.destroy();
    await stopProc(proc);
  }
});

test("server stores high-bit metric bytes as U+FFFD like ascii replace", { timeout: 60000 }, async () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "ms-e2e-metric-"));
  const db = path.join(tmp, "sim.db");
  const { proc, sock } = await spawnServer(0, db);
  try {
    const reader = new LineReader(sock);
    await drainBroadcast(reader);
    // "metric caf\xe9 ..." -> the 0xE9 byte must decode to U+FFFD, never the
    // lossy "ascii" bit-mask (0xE9 & 0x7F == 0x69 == "i").
    sock.write(Buffer.from("metric caf\xe9 t=1\n", "latin1"));
    await new Promise((r) => setTimeout(r, 500));
  } finally {
    sock.destroy();
    await stopProc(proc);
  }
  const dbc = new DatabaseSync(db, { readBigInts: true });
  try {
    const rows = readAll(dbc, "SELECT line FROM client_metrics ORDER BY id");
    const withHighBit = rows.filter((r) => r.line.includes("caf"));
    assert.ok(withHighBit.length >= 1, "high-bit metric line stored");
    assert.ok(withHighBit.some((r) => r.line.includes("\uFFFD")), "byte decoded to U+FFFD");
    assert.ok(!withHighBit.some((r) => /caf[i]/.test(r.line)), "no lossy bit-masking");
  } finally {
    dbc.close();
  }
});

function runCli(args) {
  return new Promise((resolve) => {
    const proc = spawn(
      process.execPath,
      [path.join(MS_DIR, "sim-server", "server.js"), ...args],
      { stdio: ["ignore", "pipe", "pipe"] }
    );
    let out = "";
    let err = "";
    proc.stdout.on("data", (d) => (out += d.toString()));
    proc.stderr.on("data", (d) => (err += d.toString()));
    proc.on("exit", (code) => resolve({ code, out, err }));
  });
}

test("server CLI validates numeric args like argparse", { timeout: 30000 }, async () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "ms-e2e-cli-"));
  const db = path.join(tmp, "sim.db");
  const bad = [
    ["--seed", "abc"],
    ["--port", "abc"],
    ["--rate", "abc"],
    ["--max-request", "1e3"],
    ["--max-concurrent", "0x10"],
  ];
  for (const [flag, value] of bad) {
    const r = await runCli(["--db", db, flag, value]);
    assert.equal(r.code, 2, `${flag} ${value} exits 2`);
    assert.ok(
      /invalid --(seed|port|rate|max-request|max-concurrent)/.test(r.err),
      `${flag} ${value} reports the offending option`
    );
  }
  // sanity: a valid invocation still starts (spawnServer itself uses a
  // decimal float --rate and integer --seed 99)
  const { proc, sock } = await spawnServer(0, db);
  try {
    const reader = new LineReader(sock);
    await drainBroadcast(reader);
  } finally {
    sock.destroy();
    await stopProc(proc);
  }
});
