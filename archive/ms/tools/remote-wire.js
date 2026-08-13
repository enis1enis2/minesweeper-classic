// remote-wire.js - wire-protocol smoke test for a *remote* sim server.
//
// Runs the same auth / broadcast / metrics / leaderboard / reqseed / reqbatch
// exercise as test/server-e2e.test.js against an arbitrary host:port, so the
// deployed Node server (28571) can be verified the same way.  Solver
// credentials come from --user/--pass or the MS_SOLVER_USER/MS_SOLVER_PASS
// environment variables.
//
// Usage:
//   node tools/remote-wire.js --host 127.0.0.1 --port 28571
//   exit 0 = PASS, 1 = FAIL
//
// The player/metric markers are prefixed with "re2e" so test rows can be
// identified and cleaned up afterwards.

import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import {
  LineReader,
  drainBroadcast,
  authHandshake,
  expect,
  expectAny,
} from "../test/helpers/e2e.js";

const MARK = "re2e_" + Date.now().toString(36);

function fail(...args) {
  console.error("  FAIL " + args.join(" "));
  process.exitCode = 1;
}

async function connect(host, port) {
  for (let i = 0; i < 100; i++) {
    try {
      return await new Promise((resolve, reject) => {
        const s = net.connect(port, host, () => {
          s.removeListener("error", reject);
          resolve(s);
        });
        s.once("error", reject);
      });
    } catch {
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  throw new Error("could not connect to " + host + ":" + port);
}

async function run(host, port, user, pass) {
  console.log(
    `== remote-wire ${host}:${port}  solver=${user ? "on" : "off"}  mark=${MARK}`
  );
  let step = 0;
  const ok = (what) => console.log(`  ok ${++step}. ${what}`);
  const solverEnabled = Boolean(user && pass);

  const sock = await connect(host, port);
  const reader = new LineReader(sock);
  try {
    // 1. broadcast stream produces a matching seed/outcome pair.
    const [seed, outcome] = await drainBroadcast(reader);
    if (seed[1] !== outcome[1] || seed[2] !== outcome[2]) {
      fail("broadcast seed/outcome mismatch");
    } else {
      ok(`broadcast seed ${seed[1]}/${seed[2]} -> outcome ${outcome[3]}`);
    }

    // 2. metric ingest.
    sock.write(`metric ${MARK} start diff=beginner seed=1 seeded=1 t=1\n`);
    sock.write(
      `metric ${MARK} win diff=beginner seed=1 seeded=1 time=42 clicks=10 latency=123 t=2\n`
    );
    await new Promise((r) => setTimeout(r, 500));
    ok("metric lines written");

    // 3. leaderboard submit / improve / slower / invalid name.
    sock.write(`lbscore ${MARK}A beginner 45000\n`);
    let parts = await expect(reader, "lbstored", "lbstored");
    if (parts[3] !== `${MARK}A` || parts[4] !== "45000") fail("lb submit row");
    else ok("lbscore submit stored");
    sock.write(`lbscore ${MARK}A beginner 40000\n`);
    parts = await expect(reader, "lbstored", "lbstored");
    if (parts[4] !== "40000") fail("lb improve");
    else ok("lbscore improve stored");
    sock.write(`lbscore ${MARK}A beginner 42000\n`);
    await expect(reader, "lbnotop", "lbnotop");
    ok("slower score rejected (lbnotop)");
    sock.write(`lbscore 'bad name!' beginner 1000\n`);
    sock.write("lbtop 10\n");
    parts = await expect(reader, "lbtop", "lbtop header");
    const nRows = Number(parts[1]);
    ok(`lbtop header (${nRows} rows)`);
    const rows = [];
    const deadline = Date.now() + 3000;
    while (rows.length < nRows && Date.now() < deadline) {
      const line = await Promise.race([
        reader.readLine().catch(() => "__CLOSED__"),
        new Promise((r) => setTimeout(() => r("__TIMEOUT__"), 1500)),
      ]);
      if (line === "__TIMEOUT__") continue;
      const p = line.split(/\s+/);
      if (p[0] === "lbentry") rows.push(p.slice(1, 6).join(" "));
    }
    await expect(reader, "lbdone", "lbdone");
    if (!rows.some((r) => r.includes(`${MARK}A`) && r.includes("40000"))) {
      fail("improved row not in top list");
    } else ok("improved row present in lbtop");
    if (rows.some((r) => r.includes("bad name"))) fail("invalid name stored");
    else ok("invalid name ignored");

    // 4. solver gate (only meaningful when creds are configured).
    if (solverEnabled) {
      sock.write("reqseed beginner 12345\n");
      const denied = await expect(reader, "reqdenied", "reqdenied");
      if (denied[0] !== "reqdenied") fail("unauthed reqseed not denied");
      else ok("reqseed denied before auth");

      // wrong user
      const wrongUser = await authHandshake(sock, reader, "nobody", pass);
      if (wrongUser) fail("wrong user accepted");
      else ok("wrong user rejected");

      // wrong password (fresh challenge on the same connection)
      const wrongPass = await authHandshake(sock, reader, user, "wrong-pass-1");
      if (wrongPass) fail("wrong password accepted");
      else ok("wrong password rejected");

      // correct credentials
      const okAuth = await authHandshake(sock, reader, user, pass);
      if (!okAuth) fail("correct credentials rejected");
      else ok("auth handshake accepted");

      // reqseed + reqbatch after auth
      sock.write("reqseed beginner 12345\n");
      await expectAny(reader, ["reqwait", "reqgame"], "reqseed start");
      let done = await expect(reader, "reqdone", "reqdone");
      if (done[2] !== "1") fail("reqseed count");
      else ok("reqseed beginner 12345 replayed");
      sock.write("reqbatch beginner 3\n");
      await expectAny(reader, ["reqwait", "reqgame"], "reqbatch start");
      done = await expect(reader, "reqdone", "reqdone");
      if (done[2] !== "3") fail("reqbatch count");
      else ok("reqbatch beginner 3 served");
    } else {
      sock.write("reqseed beginner 12345\n");
      await expect(reader, "reqdenied", "reqdenied");
      ok("solver disabled: reqseed denied");
    }

  } finally {
    sock.destroy();
  }
}

const { values } = parseArgs({
  options: {
    host: { type: "string", default: "127.0.0.1" },
    port: { type: "string", default: "28571" },
    user: { type: "string" },
    pass: { type: "string" },
  },
  allowPositionals: true,
});

const host = values.host;
const port = Number(values.port);
const user = values.user ?? process.env.MS_SOLVER_USER ?? null;
const pass = values.pass ?? process.env.MS_SOLVER_PASS ?? null;

if (!Number.isInteger(port) || port <= 0 || port > 65535) {
  console.error("invalid --port: " + values.port);
  process.exit(2);
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);

if (isMain) {
  try {
    await run(host, port, user, pass);
    if (process.exitCode !== 1) console.log("PASS " + host + ":" + port);
  } catch (e) {
    fail("unexpected: " + (e && e.stack ? e.stack : e));
  }
}

export { run };
