// bench-host.js - latency/throughput benchmark for a sim server.
//
// Speaks the same wire protocol as tools/remote-wire.js but measures how fast
// reqbatch games are served, so the Node (:28572) and Python (:28571) servers
// can be compared head-to-head.  Broadcast seed/outcome/metric lines are
// ignored while a batch is in flight.
//
//   --single <diff> <count> [iters]
//       One authenticated connection: a warmup batch, then `iters` measured
//       batches.  Reports games/sec (min/avg/max) and time-to-first-reqgame.
//
//   --concurrent <diff> <count> <k>
//       `k` authenticated connections each play `count` games at once.
//       Reports wall time + aggregate games/sec (scales with max-concurrent).
//
// Credentials come from --user/--pass or MS_SOLVER_USER/MS_SOLVER_PASS env.
// Usage:
//   node tools/bench-host.js --host H --port P --single beginner 100 3
//   node tools/bench-host.js --host H --port P --concurrent expert 25 4
// exit 0 = OK, 1 = FAIL, 2 = usage

import net from "node:net";
import { performance } from "node:perf_hooks";
import { parseArgs } from "node:util";
import { LineReader, authHandshake } from "../test/helpers/e2e.js";

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

// Play one reqbatch and time it.  Returns { firstMs, doneMs, played }.
async function playBatch(sock, reader, diff, count) {
  const t0 = performance.now();
  sock.write(`reqbatch ${diff} ${count}\n`);
  let firstAt = null;
  let doneAt = null;
  let played = 0;
  const deadline = Date.now() + 10 * 60 * 1000;
  while (Date.now() < deadline) {
    const parts = (await reader.readLine()).split(/\s+/);
    if (!parts.length) continue;
    const cmd = parts[0];
    if (cmd === "reqgame") {
      if (firstAt === null) firstAt = performance.now();
    } else if (cmd === "reqdone") {
      played = Number(parts[2]);
      doneAt = performance.now();
      break;
    } else if (cmd === "reqdenied") {
      throw new Error("reqdenied: solver disabled or not authenticated");
    }
  }
  if (doneAt === null) throw new Error("no reqdone for reqbatch " + diff + " " + count);
  return { firstMs: firstAt - t0, doneMs: doneAt - t0, played };
}

async function openAuthed(host, port, user, pass) {
  const sock = await connect(host, port);
  const reader = new LineReader(sock);
  if (!(await authHandshake(sock, reader, user, pass))) {
    sock.destroy();
    throw new Error("auth failed");
  }
  return { sock, reader };
}

async function warmup(host, port, user, pass, diff, count) {
  const { sock, reader } = await openAuthed(host, port, user, pass);
  await playBatch(sock, reader, diff, count);
  sock.destroy();
  reader.close();
}

async function runSingle(host, port, user, pass, diff, count, iters) {
  const warm = Math.max(5, Math.floor(count / 10));
  await warmup(host, port, user, pass, diff, warm);
  const { sock, reader } = await openAuthed(host, port, user, pass);
  const rates = [];
  const firsts = [];
  try {
    for (let i = 0; i < iters; i++) {
      const r = await playBatch(sock, reader, diff, count);
      if (r.played !== count) {
        fail(`played=${r.played} != count=${count}`);
        continue;
      }
      rates.push((count * 1000) / r.doneMs);
      firsts.push(r.firstMs);
    }
  } finally {
    sock.destroy();
    reader.close();
  }
  const avg = rates.reduce((a, b) => a + b, 0) / rates.length;
  const min = Math.min(...rates);
  const max = Math.max(...rates);
  const firstAvg = firsts.reduce((a, b) => a + b, 0) / firsts.length;
  console.log(
    `BENCH SINGLE ${diff} count=${count} iters=${iters} ` +
      `g/s avg=${avg.toFixed(1)} min=${min.toFixed(1)} max=${max.toFixed(1)} ` +
      `first=${firstAvg.toFixed(1)}ms`
  );
  return { avg, min, max, firstAvg };
}

async function runConcurrent(host, port, user, pass, diff, count, k) {
  const conns = [];
  try {
    for (let i = 0; i < k; i++) {
      const c = await openAuthed(host, port, user, pass);
      conns.push(c);
    }
    await Promise.all(
      conns.map(({ sock, reader }) => playBatch(sock, reader, diff, Math.max(5, Math.floor(count / 4))))
    );
    const t0 = performance.now();
    const results = await Promise.all(
      conns.map(({ sock, reader }) => playBatch(sock, reader, diff, count))
    );
    const wall = performance.now() - t0;
    const total = results.reduce((a, r) => a + r.played, 0);
    console.log(
      `BENCH CONCURRENT ${diff} count=${count} k=${k} wall=${wall.toFixed(0)}ms ` +
        `g/s=${((total * 1000) / wall).toFixed(1)} total=${total}`
    );
    return { wall, gps: (total * 1000) / wall, total };
  } finally {
    for (const { sock, reader } of conns) {
      sock.destroy();
      reader.close();
    }
  }
}

const { values, positionals } = parseArgs({
  options: {
    host: { type: "string", default: "127.0.0.1" },
    port: { type: "string", default: "28572" },
    user: { type: "string" },
    pass: { type: "string" },
    single: { type: "boolean", default: false },
    concurrent: { type: "boolean", default: false },
  },
  allowPositionals: true,
});

const mode = values.single ? "single" : values.concurrent ? "concurrent" : null;
const [diff, countStr, nStr] = positionals;
const DIFFS = ["beginner", "intermediate", "expert"];
if (!mode || !DIFFS.includes(diff) || !countStr) {
  console.error(
    "usage: node tools/bench-host.js --host H --port P [--user U --pass X] " +
      "(--single <diff> <count> [iters] | --concurrent <diff> <count> <k>)"
  );
  process.exit(2);
}
const count = Number(countStr);
const n = Number(nStr || (mode === "single" ? 3 : 4));
if (!Number.isInteger(count) || count < 1) process.exit(2);
if (!Number.isInteger(n) || n < 1) process.exit(2);

const host = values.host;
const port = Number(values.port);
if (!Number.isInteger(port) || port <= 0 || port > 65535) {
  console.error("invalid --port: " + values.port);
  process.exit(2);
}
const user = values.user ?? process.env.MS_SOLVER_USER ?? null;
const pass = values.pass ?? process.env.MS_SOLVER_PASS ?? null;
if (!user || !pass) {
  console.error(
    "bench-host: solver credentials required (--user/--pass or MS_SOLVER_USER/MS_SOLVER_PASS)"
  );
  process.exit(2);
}

try {
  if (mode === "single") await runSingle(host, port, user, pass, diff, count, n);
  else if (mode === "concurrent") await runConcurrent(host, port, user, pass, diff, count, n);
  else process.exit(2);
} catch (e) {
  fail("unexpected: " + (e && e.stack ? e.stack : e));
}

export { runSingle, runConcurrent };
