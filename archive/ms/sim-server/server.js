// server.js - Minesweeper simulation server (CLI entry).
//
// Zero-dependency port of server/ms_server.py.  Run:
//   node sim-server/server.js [--host 0.0.0.0] [--port 28571] [--db data/sim.db]
//       [--rate 5] [--difficulty all|beginner|intermediate|expert]
//       [--seed 12345] [--max-request 10000] [--max-concurrent 1]
//       [--solver-user USER --solver-pass PASS | --solver-config FILE]
//   node sim-server/server.js --selfcheck

import net from "node:net";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { Random } from "../core/mt19937.js";
import { DIFFS } from "./config.js";
import { Database } from "./database.js";
import { ClientHub, RequestWorkers, AdmissionGate } from "./hub.js";
import { WorkerPool } from "./worker-pool.js";
import {
  handleConn,
  handleRequest,
  produce,
} from "./protocol.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));

function resolveSolver(args) {
  let user = args["solver-user"];
  let pw = args["solver-pass"];
  if (args["solver-config"]) {
    const data = JSON.parse(fs.readFileSync(args["solver-config"], "utf8"));
    user = user || (data.user ?? null);
    pw = pw || (data.pass ?? null);
  }
  user = user || process.env.MS_SOLVER_USER || null;
  pw = pw || process.env.MS_SOLVER_PASS || null;
  return [user, pw];
}

async function run(args) {
  const [solverUser, solverPass] = resolveSolver(args);
  const solverEnabled = Boolean(solverUser && solverPass);

  const diffs =
    args.difficulty !== "all"
      ? args.difficulty.split(",")
      : [...DIFFS];
  for (const d of diffs) {
    if (!DIFFS.includes(d)) {
      console.error(
        `unknown difficulty ${JSON.stringify(d)} (use all|beginner|intermediate|expert)`
      );
      return 2;
    }
  }

  const db = new Database(args.db);
  const server = {
    stop: false,
    db,
    hub: null,
    diffs,
    rate: args.rate,
    maxRequest: args["max-request"],
    gate: new AdmissionGate(args["max-concurrent"]),
    reqWorkers: null,
    solverUser,
    solverPass,
    solverEnabled,
    lbHist: {},
    pool: null,
  };
  server.handleRequest = handleRequest.bind(null, server);
  server.hub = new ClientHub(db);
  server.reqWorkers = new RequestWorkers(server);
  // max-concurrent heavy requests run at once; keep headroom so light
  // requests and the broadcast producer never wait on a heavy one.
  server.pool = new WorkerPool(args["max-concurrent"] + 2);

  const rng = new Random(args.seed ?? null);

  const listener = net.createServer((conn) => handleConn(server, conn));
  listener.on("error", (e) => {
    console.error(`listen error: ${e.message}`);
    server.stop = true;
  });
  await new Promise((resolve, reject) => {
    listener.once("error", reject);
    listener.listen(args.port, args.host, () => {
      listener.removeListener("error", reject);
      resolve();
    });
  });

  const boundPort = listener.address().port;
  console.log(
    `ms_server listening on ${args.host}:${boundPort}  (db=${args.db}  ` +
      `rate=${args.rate.toFixed(1)} g/s  max-concurrent=${args["max-concurrent"]}  ` +
      `solver=${solverEnabled ? "protected" : "disabled"})`
  );

  void produce(server, rng).catch((e) => {
    console.error(`producer error: ${e && e.stack ? e.stack : e}`);
  });

  const statusTimer = setInterval(() => {
    const [g, m, c] = db.counts();
    console.log(
      `  games=${g[0]} wins=${g[1]} metrics=${m[0]} clients=${c}`
    );
  }, 1000);

  const shutdown = async () => {
    if (server.stop) return;
    server.stop = true;
    console.log("\nshutting down...");
    clearInterval(statusTimer);
    try {
      listener.close();
    } catch {
      // already closed
    }
    await sleep(200);
    await server.pool.close();
    db.close();
  };
  process.on("SIGINT", () => void shutdown());
  process.on("SIGTERM", () => void shutdown());

  while (!server.stop) {
    await sleep(250);
  }
  return 0;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function usage() {
  console.log(
    "usage: node sim-server/server.js [--host HOST] [--port PORT] [--db FILE] " +
      "[--rate G/S] [--difficulty all|beginner|intermediate|expert] " +
      "[--seed N] [--max-request N] [--max-concurrent N] " +
      "[--solver-user USER --solver-pass PASS | --solver-config FILE] [--selfcheck]"
  );
}

async function main() {
  const { values, positionals } = parseArgs({
    options: {
      host: { type: "string", default: "0.0.0.0" },
      port: { type: "string", default: "28571" },
      db: { type: "string", default: path.join(HERE, "data", "sim.db") },
      rate: { type: "string", default: "5.0" },
      difficulty: { type: "string", default: "all" },
      seed: { type: "string" },
      "max-request": { type: "string", default: "10000" },
      "max-concurrent": { type: "string", default: "1" },
      "solver-user": { type: "string" },
      "solver-pass": { type: "string" },
      "solver-config": { type: "string" },
      selfcheck: { type: "boolean", default: false },
      help: { type: "boolean", default: false },
    },
    allowPositionals: true,
  });

  if (values.help || positionals.length) {
    usage();
    return values.help ? 0 : 2;
  }

  // argparse type=int / type=float validation (exit 2 on malformed values)
  const INT_VAL = /^-?[0-9]+$/;
  const FLOAT_VAL = /^-?[0-9]+(\.[0-9]+)?$/;
  if (!INT_VAL.test(values.port)) {
    console.error(`ms_server: error: invalid --port value: '${values.port}'`);
    return 2;
  }
  if (!FLOAT_VAL.test(values.rate) || !Number.isFinite(Number(values.rate))) {
    console.error(`ms_server: error: invalid --rate value: '${values.rate}'`);
    return 2;
  }
  if (!INT_VAL.test(values["max-request"])) {
    console.error(
      `ms_server: error: invalid --max-request value: '${values["max-request"]}'`
    );
    return 2;
  }
  if (!INT_VAL.test(values["max-concurrent"])) {
    console.error(
      `ms_server: error: invalid --max-concurrent value: '${values["max-concurrent"]}'`
    );
    return 2;
  }
  let seed = null;
  if (values.seed !== undefined) {
    if (!INT_VAL.test(values.seed)) {
      console.error(`ms_server: error: invalid --seed value: '${values.seed}'`);
      return 2;
    }
    seed = BigInt(values.seed);
  }

  const args = {
    host: values.host,
    port: Number(values.port),
    db: values.db,
    rate: Number(values.rate),
    difficulty: values.difficulty,
    seed,
    "max-request": Number(values["max-request"]),
    "max-concurrent": Number(values["max-concurrent"]),
    "solver-user": values["solver-user"] ?? null,
    "solver-pass": values["solver-pass"] ?? null,
    "solver-config": values["solver-config"] ?? null,
  };

  if (values.selfcheck) {
    const { selfcheck } = await import("./selfcheck.js");
    return selfcheck();
  }
  return run(args);
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  process.exit(await main());
}

export { run, main, resolveSolver };
