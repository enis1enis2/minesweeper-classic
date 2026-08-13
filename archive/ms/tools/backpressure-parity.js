#!/usr/bin/env node
// backpressure-parity.js - cross-language scheduling/backpressure differential.
//
// Drives the Python sim server (server/ms_server.py) and the Node sim server
// (ms/sim-server/server.js) through their real TCP entry points with the SAME
// synthetic load, then asserts both implementations make the same admission
// decisions and enforce the same backpressure contract.
//
// Load (per server, over four authed connections):
//     A, B, C each send  reqbatch expert 4        (heavy: 4*0.076s = 0.304s >= 0.25s)
//     L sends            reqbatch intermediate 1  (light: 0.016s < 0.25s, bypasses the gate)
//
// The per-client request worker is FIFO, so with --max-concurrent 2 the gate
// admits two heavy requests immediately and queues the third until one of the
// first two finishes.  Every heavy request announces itself with a "reqwait"
// line before acquiring a permit; light requests never do.  A request's first
// "reqgame" line is the moment it is admitted past the gate; its "reqdone"
// line is the moment it releases its permit.
//
// Contract (identical for both ports; mirrors the AdmissionGate + per-client
// RequestWorkers semantics in server/ms_server.py and ms/sim-server/hub.js):
//   1. every heavy request (A, B, C) gets exactly one "reqwait" line, then
//      exactly 4 reqgame lines, then one "reqdone" line, never a "reqdenied"
//   2. the light request gets no "reqwait" and completes with its own single
//      reqgame + reqdone (proof that it bypassed the gate)
//   3. the gate admits exactly two heavy requests before any heavy request
//      finishes: the merged shape starts with "GG"
//   4. the third heavy request can only start after a permit frees, so the
//      merged shape has exactly three Gs and three Ds, never more than two
//      heavy requests in flight, and ends with a completion "D" (FIFO permit
//      release on reqdone; the queued request is admitted on the first done)
//   5. backpressure: at every prefix of the merged trace, the number of heavy
//      requests that have started but not finished never exceeds 2
//   6. the per-actor summaries and gate invariants are IDENTICAL on both
//      servers (the cross-language parity assertion).  The exact G/D
//      interleaving is NOT compared across servers: a release (reqdone on the
//      completing client's socket) and the next admission (reqgame on the
//      waiting client's socket) race in client-side arrival order, so only the
//      structural invariants above are deterministic.
//
// Broadcast producer lines ("seed"/"outcome") are ignored: they never carry
// reqwait/reqgame/reqdone, so the classifier is immune to broadcast noise and
// to the --rate pacing.  Auth uses the HMAC-SHA256 challenge handshake
// (auth / authchal / authresp / authok) shared by both ports.
//
// Time budget: two expert*4 batches run concurrently (~1s), the third waits
// for a permit then runs (~1s) -> a few seconds of real solver work per server.
//
// Exit code: 0 = parity held, 1 = mismatch or harness error.
// Requires `python` on PATH for the Python side.
//
// Usage: node tools/backpressure-parity.js

import { spawn } from "node:child_process";
import net from "node:net";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const MS_DIR = path.dirname(here);
const REPO = path.dirname(MS_DIR);
const PYTHON = process.env.MS_PYTHON || "python";

const USER = "ms";
const PASS = "change-me";
const MAX_CONCURRENT = 2;
const RATE = "2"; // paced broadcast; the classifier ignores broadcast lines
const HEAVY = "reqbatch expert 4"; // 0.304s >= HEAVY_CPU_SECONDS(0.25) -> gated
const LIGHT = "reqbatch intermediate 1"; // 0.016s < 0.25s -> bypasses the gate
const HEAVY_ACTORS = ["A", "B", "C"];
const ALL_ACTORS = ["A", "B", "C", "L"];
const GATE_KINDS = ["reqwait", "reqgame", "reqdone", "reqdenied"];
const EXPECTED_INVARIANTS = "GG" + HEAVY_ACTORS.length + HEAVY_ACTORS.length + "LE"; // GG + 3 Gs + 3 Ds + permit(<=2) + ends with D

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------------------------------------------------------------------------
// server plumbing
// ---------------------------------------------------------------------------

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const p = srv.address().port;
      srv.close(() => resolve(p));
    });
  });
}

async function waitPort(port, proc, getErr) {
  const deadline = Date.now() + 20000;
  while (Date.now() < deadline) {
    if (proc.exitCode !== null) {
      throw new Error(
        "server exited early (code " + proc.exitCode + "): " + getErr()
      );
    }
    const ok = await new Promise((resolve) => {
      const s = net.connect(port, "127.0.0.1");
      s.once("connect", () => {
        s.destroy();
        resolve(true);
      });
      s.once("error", () => resolve(false));
    });
    if (ok) return;
    await sleep(150);
  }
  throw new Error("server did not open port " + port + ": " + getErr());
}

async function startServer(lang) {
  const port = await freePort();
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "bpp-" + lang + "-"));
  const common = [
    "--host",
    "127.0.0.1",
    "--port",
    String(port),
    "--db",
    path.join(tmp, "sim.db"),
    "--rate",
    RATE,
    "--max-concurrent",
    String(MAX_CONCURRENT),
    "--solver-user",
    USER,
    "--solver-pass",
    PASS,
  ];
  const argv =
    lang === "python"
      ? [path.join(REPO, "server", "ms_server.py"), ...common]
      : [path.join(MS_DIR, "sim-server", "server.js"), ...common];
  const proc = spawn(lang === "python" ? PYTHON : process.execPath, argv, {
    cwd: REPO,
    stdio: ["ignore", "ignore", "pipe"],
  });
  let stderr = "";
  proc.stderr.on("data", (d) => {
    stderr += d.toString();
  });
  await waitPort(port, proc, () => stderr);
  return {
    lang,
    port,
    proc,
    stderr: () => stderr,
    cleanup: () => {
      try {
        proc.kill();
      } catch {
        // already dead
      }
      try {
        fs.rmSync(tmp, { recursive: true, force: true });
      } catch {
        // temp dir already gone
      }
    },
  };
}

// ---------------------------------------------------------------------------
// client plumbing
// ---------------------------------------------------------------------------

class LineClient {
  constructor(port, onLine, onError) {
    this.buf = "";
    this.closed = false;
    this.sock = net.connect(port, "127.0.0.1");
    this.sock.on("data", (d) => {
      this.buf += d.toString("utf8");
      let nl;
      while ((nl = this.buf.indexOf("\n")) >= 0) {
        const raw = this.buf.slice(0, nl);
        this.buf = this.buf.slice(nl + 1);
        const text = raw.replace(/\r$/, "").trim();
        if (text) onLine(text);
      }
    });
    this.sock.on("error", (e) => onError(e));
    this.sock.on("close", () => {
      this.closed = true;
      onError(new Error("connection closed"));
    });
  }

  send(line) {
    return new Promise((res, rej) =>
      this.sock.write(line + "\n", (e) => (e ? rej(e) : res()))
    );
  }

  close() {
    if (!this.closed) this.sock.end();
  }
}

// Connect to a server, complete the solver-auth challenge (auth -> authchal
// -> authresp -> authok; HMAC-SHA256(secret, "ms-auth:" + nonce) hex), then
// route every further line to `onLine`.  Broadcast seed/outcome lines that
// arrive mid-handshake are ignored.  Resolves to the client.
function connectAuthed(port, onLine) {
  return new Promise((resolve, reject) => {
    let authed = false;
    let timer;
    const cli = new LineClient(
      port,
      (line) => {
        if (!authed) {
          const p = line.split(/\s+/);
          if (p[0] === "authchal") {
            const digest = crypto
              .createHmac("sha256", PASS)
              .update("ms-auth:" + p[1])
              .digest("hex");
            cli.send("authresp " + digest).catch((e) => reject(e));
          } else if (p[0] === "authok") {
            authed = true;
            clearTimeout(timer);
            resolve(cli);
          } else if (p[0] === "autherr") {
            clearTimeout(timer);
            reject(new Error("auth rejected"));
          }
          return;
        }
        onLine(line);
      },
      (e) => {
        clearTimeout(timer);
        reject(e);
      }
    );
    cli.send("auth " + USER).catch((e) => reject(e));
    timer = setTimeout(
      () => reject(new Error("auth handshake timed out")),
      10000
    );
  });
}

// ---------------------------------------------------------------------------
// scenario driver
// ---------------------------------------------------------------------------

// Drive the load against one server.  Resolves to:
//   { events: [{ k, kind, t }] in arrival order, gate-relevant kinds only,
//     lines:  { A: [...], B: [...], C: [...], L: [...] } raw lines }
async function runScenario(label, port) {
  const lines = {};
  for (const k of ALL_ACTORS) lines[k] = [];
  const events = [];
  const conns = {};
  let haveDone = 0;
  const doneSeen = {};
  let settled = false;
  let resolveRun, rejectRun;
  const promise = new Promise((res, rej) => {
    resolveRun = res;
    rejectRun = rej;
  });
  const timer = setTimeout(
    () => finish(new Error(label + ": timed out waiting for all reqdone")),
    90000
  );

  function finish(err) {
    if (settled) return;
    settled = true;
    clearTimeout(timer);
    for (const k of ALL_ACTORS) {
      if (conns[k]) conns[k].close();
    }
    if (err) rejectRun(err);
    else resolveRun({ lines, events });
  }

  const handle = (k) => (line) => {
    lines[k].push(line);
    const p = line.split(/\s+/)[0];
    if (GATE_KINDS.includes(p)) {
      events.push({ k, kind: p, t: Date.now() });
    }
    if (p === "reqdone" && !doneSeen[k]) {
      doneSeen[k] = true;
      haveDone += 1;
      if (haveDone === ALL_ACTORS.length) finish(null);
    } else if (p === "reqdenied") {
      finish(new Error(label + ": " + k + " got reqdenied: " + line));
    }
  };

  try {
    for (const k of ALL_ACTORS) {
      conns[k] = await connectAuthed(port, handle(k));
    }
  } catch (e) {
    finish(new Error(label + ": " + e.message));
    return promise;
  }
  // Fire the load: heavy first (A, B, C in order), light last.
  await conns.A.send(HEAVY);
  await conns.B.send(HEAVY);
  await conns.C.send(HEAVY);
  await conns.L.send(LIGHT);
  return promise;
}

// ---------------------------------------------------------------------------
// assertions
// ---------------------------------------------------------------------------

function gateShape(events) {
  const firstGameSeen = {};
  for (const k of HEAVY_ACTORS) firstGameSeen[k] = false;
  const shape = [];
  for (const ev of events) {
    if (!HEAVY_ACTORS.includes(ev.k)) continue;
    if (ev.kind === "reqgame" && !firstGameSeen[ev.k]) {
      firstGameSeen[ev.k] = true;
      shape.push("G");
    } else if (ev.kind === "reqdone") {
      shape.push("D");
    }
  }
  return shape.join("");
}

function analyze(label, trace) {
  const errs = [];
  const { lines, events } = trace;

  // per-actor counts + same-socket reqwait-before-first-reqgame ordering
  const actors = {};
  for (const k of ALL_ACTORS) {
    const a = { reqwait: 0, reqgame: 0, reqdone: 0, reqdenied: 0, firstWait: -1, firstGame: -1 };
    lines[k].forEach((line, i) => {
      const p = line.split(/\s+/)[0];
      if (p === "reqwait") {
        a.reqwait += 1;
        if (a.firstWait < 0) a.firstWait = i;
      } else if (p === "reqgame") {
        a.reqgame += 1;
        if (a.firstGame < 0) a.firstGame = i;
      } else if (p === "reqdone") {
        a.reqdone += 1;
      } else if (p === "reqdenied") {
        a.reqdenied += 1;
      }
    });
    actors[k] = a;
  }

  for (const k of HEAVY_ACTORS) {
    const a = actors[k];
    if (a.reqwait !== 1)
      errs.push(`${label} ${k}: reqwait=${a.reqwait}, want 1`);
    if (a.reqgame !== 4)
      errs.push(`${label} ${k}: reqgame=${a.reqgame}, want 4`);
    if (a.reqdone !== 1)
      errs.push(`${label} ${k}: reqdone=${a.reqdone}, want 1`);
    if (a.reqdenied !== 0)
      errs.push(`${label} ${k}: reqdenied=${a.reqdenied}, want 0`);
    if (a.firstWait < 0 || a.firstGame < 0 || a.firstWait >= a.firstGame)
      errs.push(
        `${label} ${k}: reqwait(index ${a.firstWait}) must precede first reqgame(index ${a.firstGame})`
      );
  }
  const L = actors.L;
  if (L.reqwait !== 0)
    errs.push(`${label} L: light request got reqwait=${L.reqwait}, want 0 (must bypass the gate)`);
  if (L.reqgame !== 1)
    errs.push(`${label} L: reqgame=${L.reqgame}, want 1`);
  if (L.reqdone !== 1)
    errs.push(`${label} L: reqdone=${L.reqdone}, want 1`);
  if (L.reqdenied !== 0)
    errs.push(`${label} L: reqdenied=${L.reqdenied}, want 0`);

  // merged heavy gate timeline: first-reqgame = admitted, reqdone = released.
  const firstGameSeen = {};
  for (const k of HEAVY_ACTORS) firstGameSeen[k] = false;
  let inFlight = 0;
  let maxInFlight = 0;
  let heavyDone = 0;
  const shape = [];
  for (const ev of events) {
    if (!HEAVY_ACTORS.includes(ev.k)) continue;
    if (ev.kind === "reqgame" && !firstGameSeen[ev.k]) {
      firstGameSeen[ev.k] = true;
      inFlight += 1;
      maxInFlight = Math.max(maxInFlight, inFlight);
      shape.push("G");
    } else if (ev.kind === "reqdone") {
      inFlight -= 1;
      heavyDone += 1;
      shape.push("D");
    }
  }
  if (inFlight !== 0)
    errs.push(`${label}: heavy in flight at end ${inFlight}, want 0`);
  if (heavyDone !== HEAVY_ACTORS.length)
    errs.push(`${label}: heavy reqdone count ${heavyDone}, want ${HEAVY_ACTORS.length}`);

  // Structural gate invariants.  These are deterministic even though the exact
  // G/D interleaving is racy across independent sockets (see header).
  const shapeStr = shape.join("");
  const gCount = (shapeStr.match(/G/g) || []).length;
  const dCount = (shapeStr.match(/D/g) || []).length;
  const startsGG = shapeStr.startsWith("GG");
  const permitsOk = maxInFlight <= MAX_CONCURRENT;
  const endsD = shapeStr.endsWith("D");
  if (!startsGG)
    errs.push(
      `${label}: gate shape ${JSON.stringify(shapeStr)} must start "GG" (two heavy requests admitted before any completes)`
    );
  if (gCount !== HEAVY_ACTORS.length)
    errs.push(`${label}: gate G count ${gCount}, want ${HEAVY_ACTORS.length}`);
  if (dCount !== HEAVY_ACTORS.length)
    errs.push(`${label}: gate D count ${dCount}, want ${HEAVY_ACTORS.length}`);
  if (!permitsOk)
    errs.push(`${label}: max heavy requests in flight ${maxInFlight} > ${MAX_CONCURRENT}`);
  if (!endsD)
    errs.push(`${label}: gate shape ${JSON.stringify(shapeStr)} must end with a completion "D"`);
  const invariants =
    (startsGG ? "GG" : "--") +
    String(gCount) +
    String(dCount) +
    (permitsOk ? "L" : "H") +
    (endsD ? "E" : "-");

  const summary = HEAVY_ACTORS.concat(["L"])
    .map(
      (k) =>
        `${k}:${actors[k].reqwait}/${actors[k].reqgame}/${actors[k].reqdone}`
    )
    .join(" ");
  return { errs, shape: shapeStr, invariants, summary };
}

// ---------------------------------------------------------------------------

async function main() {
  console.log("backpressure-parity: live Python <-> Node scheduling differential");
  console.log(
    `  load: 3x "${HEAVY}" (heavy) + 1x "${LIGHT}" (light), gate --max-concurrent ${MAX_CONCURRENT}`
  );

  const py = await startServer("python");
  const nd = await startServer("node");
  const failures = [];

  try {
    let t = Date.now();
    console.log("\n[1/2] python: server/ms_server.py on 127.0.0.1:" + py.port);
    const pyRun = await runScenario("python", py.port);
    const pyA = analyze("python", pyRun);
    console.log(`  gate shape ${pyA.shape} (${Date.now() - t}ms wall)`);

    t = Date.now();
    console.log("\n[2/2] node: ms/sim-server/server.js on 127.0.0.1:" + nd.port);
    const ndRun = await runScenario("node", nd.port);
    const ndA = analyze("node", ndRun);
    console.log(`  gate shape ${ndA.shape} (${Date.now() - t}ms wall)`);

    failures.push(...pyA.errs, ...ndA.errs);
    if (pyA.invariants !== ndA.invariants)
      failures.push(
        `gate invariants differ: python=${pyA.invariants} node=${ndA.invariants}, want ${EXPECTED_INVARIANTS}`
      );
    if (pyA.invariants !== EXPECTED_INVARIANTS)
      failures.push(`python gate invariants ${pyA.invariants}, want ${EXPECTED_INVARIANTS}`);
    if (ndA.invariants !== EXPECTED_INVARIANTS)
      failures.push(`node gate invariants ${ndA.invariants}, want ${EXPECTED_INVARIANTS}`);
    if (pyA.summary !== ndA.summary)
      failures.push(
        `actor summaries differ: python=[${pyA.summary}] node=[${ndA.summary}]`
      );
    console.log(`\n  python: ${pyA.summary}`);
    console.log(`  node:   ${ndA.summary}`);
  } catch (e) {
    failures.push(e.message);
    failures.push(
      `python: port=${py.port} exitCode=${py.proc.exitCode} alive=${py.proc.exitCode === null} stderr=${JSON.stringify(py.stderr().slice(-400))}`
    );
    failures.push(
      `node: port=${nd.port} exitCode=${nd.proc.exitCode} alive=${nd.proc.exitCode === null} stderr=${JSON.stringify(nd.stderr().slice(-400))}`
    );
    const probe = async (port, who) => {
      const r = await new Promise((resolve) => {
        const s = net.connect(port, "127.0.0.1");
        s.once("connect", () => { s.destroy(); resolve("ACCEPTED"); });
        s.once("error", (e) => resolve("ERR " + e.code));
        s.setTimeout(1000, () => { s.destroy(); resolve("TIMEOUT"); });
      });
      return `${who}:${port} -> ${r}`;
    };
    failures.push(await probe(py.port, "python"));
    failures.push(await probe(nd.port, "node"));
  } finally {
    py.cleanup();
    nd.cleanup();
  }

  const ok = failures.length === 0;
  for (const f of failures) console.log("  MISMATCH: " + f);
  console.log(
    `\nbackpressure-parity: ${ok ? "PASS" : "FAIL"} (${failures.length} mismatch${failures.length === 1 ? "" : "es"})`
  );
  return ok ? 0 : 1;
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  const code = await main();
  process.exit(code);
}

export { main, runScenario, analyze, gateShape };
