// ms_analyze.js - win/loss analysis client for the telemetry server.
//
// Zero-dependency port of server/ms_analyze.py.  Uses the server's seed-request
// system to run controlled batches and collect accurate per-difficulty win/loss
// statistics for the *simulated* (solver) games, independent of the random
// broadcast stream.  Works against any host running ms-server (local or
// deployed).
//
// Usage:
//   node ms/cli/ms_analyze.js --difficulty expert --games 500 --host 127.0.0.1
//   node ms/cli/ms_analyze.js --difficulty beginner --seed 12345
//   node ms/cli/ms_analyze.js --difficulty expert --seed 12345 --multi 50
//   node ms/cli/ms_analyze.js --all --games 200 --out results.csv
//   node ms/cli/ms_analyze.js --difficulty expert --seed 12345 --until-loss
//
// The `--all` mode runs every difficulty as a batch.  `--seed` replays one exact
// seed; add `--multi N` to run it N times (same board, varied solver tie-breaks)
// so you see the seed's range of outcomes.  Every run - wins *and* losses - is
// reported and written to the CSV.  `--until-loss` keeps replaying a seed until
// a loss is observed (capped at `--multi N`, default 25), reporting how many
// runs it took and how the solver lost.  Each requested game is counted only if
// the server marks it with a `reqgame` line, so broadcast games on the same
// connection never pollute the stats.
//
// Protocol (client -> server):
//   reqbatch <difficulty> <count>
//   reqseed  <difficulty> <seed> [count]
//   requntil <difficulty> <seed> [max]
// The server answers each requested game with:
//   reqgame <diff> <seed>
//   seed    <diff> <seed>
//   outcome <diff> <seed> <won> <moves> <time_ms> <guesses>
// For requntil it adds lossfound / noloss, and closes every request with:
//   reqdone <diff> <count>

import net from "node:net";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const DIFFS = ["beginner", "intermediate", "expert"];
const GAME_FIELDS = ["difficulty", "seed", "won", "moves", "time_ms", "guesses"];

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

class LineReader {
  constructor(sock) {
    this.sock = sock;
    this.buf = "";
    this.waiters = [];
    this.closed = false;
    sock.on("data", (chunk) => {
      this.buf += chunk.toString("ascii");
      this._pump();
    });
    sock.on("close", () => {
      this.closed = true;
      this._pump();
    });
    sock.on("error", () => {});
  }

  _pump() {
    while (this.waiters.length) {
      const nl = this.buf.indexOf("\n");
      if (nl < 0) break;
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      this.waiters.shift()(line.trim());
    }
    if (this.closed) {
      for (const w of this.waiters.splice(0)) w(null);
    }
  }

  next() {
    const nl = this.buf.indexOf("\n");
    if (nl >= 0) {
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      return Promise.resolve(line.trim());
    }
    if (this.closed) {
      return Promise.resolve(null);
    }
    return new Promise((resolve) => {
      this.waiters.push(resolve);
    });
  }
}

export class Analyzer {
  constructor(host, port, timeout = 60.0) {
    this.host = host;
    this.port = port;
    this.timeout = timeout;
    this.sock = null;
    this.reader = null;
    this.lossInfo = null;
  }

  connect() {
    return new Promise((resolve, reject) => {
      const sock = net.connect(this.port, this.host, () => resolve());
      sock.once("error", reject);
      sock.setTimeout(Math.round(this.timeout * 1000));
      this.sock = sock;
    }).then(() => {
      this.reader = new LineReader(this.sock);
    });
  }

  async _readLine() {
    for (;;) {
      const line = await this.reader.next();
      if (line === null) throw new ConnectionError("server closed the connection");
      if (line !== "") return line;
    }
  }

  async _readLineWithin(ms) {
    let timer;
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("timed out")), ms);
    });
    try {
      return await Promise.race([this._readLine(), timeout]);
    } finally {
      clearTimeout(timer);
    }
  }

  async auth(user, password) {
    // Solver-auth challenge-response handshake (HMAC-SHA256 over a server
    // nonce).  Returns true on `authok`.
    if (!user) return false;
    this.sock.write("auth " + user + "\n");
    const deadline = Date.now() + 10000;
    for (;;) {
      if (Date.now() >= deadline) throw new Error("timed out during solver authentication");
      const parts = (await this._readLineWithin(deadline - Date.now())).split(/\s+/);
      if (!parts.length) continue;
      if (parts[0] === "authchal") {
        const digest = crypto
          .createHmac("sha256", password)
          .update("ms-auth:" + parts[1])
          .digest("hex");
        this.sock.write("authresp " + digest + "\n");
      } else if (parts[0] === "authok") {
        return true;
      } else if (parts[0] === "autherr" || parts[0] === "reqdenied") {
        return false;
      }
    }
  }

  async request(line, expectedCount) {
    // Send one request, drain the stream, return the finished games.
    //
    // Only games opened by a `reqgame` marker are returned.  Blocks until
    // the matching `reqdone` arrives or the timeout expires.  Any
    // `lossfound`/`noloss` marker is recorded in this.lossInfo.
    this.sock.write(line + "\n");
    const games = [];
    this.lossInfo = null;
    let pending = null; // { diff, seed } of the current requested game
    const deadline = Date.now() + 60000;
    for (;;) {
      if (Date.now() >= deadline) {
        throw new Error("timed out waiting for reqdone (" + line + ")");
      }
      const parts = (await this._readLineWithin(deadline - Date.now())).split(/\s+/);
      if (!parts.length) continue;
      if (parts[0] === "reqgame") {
        pending = { diff: parts[1], seed: BigInt(parts[2]) };
      } else if (parts[0] === "seed") {
        // re-roll marker; tracked via reqgame instead
      } else if (parts[0] === "outcome" && pending !== null) {
        if (parts[1] === pending.diff && BigInt(parts[2]) === pending.seed) {
          games.push({
            difficulty: parts[1],
            seed: BigInt(parts[2]),
            won: Number(parts[3]),
            moves: Number(parts[4]),
            time_ms: Number(parts[5]),
            guesses: Number(parts[6]),
          });
          pending = null;
        }
      } else if (parts[0] === "lossfound") {
        this.lossInfo = {
          kind: "loss",
          run: Number(parts[3]),
          won: Number(parts[4]),
          moves: Number(parts[5]),
          time_ms: Number(parts[6]),
          guesses: Number(parts[7]),
        };
      } else if (parts[0] === "noloss") {
        this.lossInfo = { kind: "noloss", max: Number(parts[3]) };
      } else if (parts[0] === "reqdenied") {
        throw new Error(
          "server denied the request (solver disabled or needs credentials; " +
            "pass --solver-user/--solver-pass)",
        );
      } else if (parts[0] === "reqdone") {
        const got = Number(parts[2]);
        if (!line.startsWith("requntil") && got !== expectedCount) {
          console.error(
            `  warning: server played ${got} of ${expectedCount} requested games`,
          );
        }
        return games;
      }
    }
  }

  close() {
    try {
      this.sock.destroy();
    } catch {
      // ignore
    }
  }
}

class ConnectionError extends Error {}

function summarize(games, label, detail = false) {
  const n = games.length;
  if (n === 0) {
    console.log(`  ${String(label).padEnd(12)} no games returned`);
    return null;
  }
  const wins = games.filter((g) => g.won).length;
  const losses = n - wins;
  const mv = games.map((g) => g.moves);
  const gs = games.map((g) => g.guesses);
  const tm = games.map((g) => g.time_ms);
  const wm = games.filter((g) => g.won).map((g) => g.moves);
  const lm = games.filter((g) => !g.won).map((g) => g.moves);
  console.log(
    `  ${String(label).padEnd(12)} games=${String(n).padEnd(5)} wins=${String(wins).padEnd(5)} ` +
      `losses=${String(losses).padEnd(5)} win_rate=${(wins / n * 100.0).toFixed(2)}%`,
  );
  if (wins && losses) {
    const wsum = (f) =>
      games.filter((g) => g.won).reduce((a, g) => a + g[f], 0);
    const lsum = (f) =>
      games.filter((g) => !g.won).reduce((a, g) => a + g[f], 0);
    console.log(
      `  ${String("").padEnd(12)} wins : avg_moves=${(sum(wm) / wins).toFixed(1)} ` +
        `avg_guesses=${(wsum("guesses") / wins).toFixed(2)} avg_time_ms=${(wsum("time_ms") / wins).toFixed(1)}`,
    );
    console.log(
      `  ${String("").padEnd(12)} losses: avg_moves=${(sum(lm) / losses).toFixed(1)} ` +
        `avg_guesses=${(lsum("guesses") / losses).toFixed(2)} avg_time_ms=${(lsum("time_ms") / losses).toFixed(1)}`,
    );
  }
  console.log(
    `  ${String("").padEnd(12)} all   : avg_moves=${(sum(mv) / n).toFixed(1)} ` +
      `avg_guesses=${(sum(gs) / n).toFixed(2)} avg_time_ms=${(sum(tm) / n).toFixed(1)}`,
  );
  if (detail) {
    console.log(`  ${String("").padEnd(12)} per-run (seed, won, moves, guesses, time_ms):`);
    for (const g of games) {
      console.log(
        `  ${String("").padEnd(12)}   ${String(g.seed).padStart(20)} ${g.won ? "win " : "LOSS"} ` +
          `${String(g.moves).padStart(5)} ${String(g.guesses).padStart(5)} ${String(g.time_ms).padStart(6)}`,
      );
    }
  }
  return {
    difficulty: label,
    games: n,
    wins,
    losses,
    win_rate: (wins / n) * 100.0,
    avg_moves: sum(mv) / n,
    avg_guesses: sum(gs) / n,
    avg_time_ms: sum(tm) / n,
  };
}

function sum(arr) {
  return arr.reduce((a, b) => a + b, 0);
}

export async function run(args) {
  const diffs = args.all || !args.difficulty ? DIFFS : [args.difficulty];
  for (const d of diffs) {
    if (!DIFFS.includes(d)) {
      console.log(`unknown difficulty ${JSON.stringify(d)} (use ${DIFFS.join("|")})`);
      return 2;
    }
  }

  if (args.untilLoss && args.seed === null) {
    console.log("--until-loss requires --seed");
    return 2;
  }

  console.log(`connecting to ${args.host}:${args.port}`);
  const a = new Analyzer(args.host, args.port);
  try {
    await a.connect();
    if (args.solverUser) {
      console.log("authenticating to the solver...");
      const ok = await a.auth(args.solverUser, args.solverPass || "");
      if (!ok) {
        console.error("solver authentication FAILED");
        return 3;
      }
    }
    const allGames = [];
    for (const diff of diffs) {
      if (args.seed !== null && args.untilLoss) {
        const maxRuns = args.multi > 0 ? args.multi : 25;
        const games = await a.request(`requntil ${diff} ${args.seed} ${maxRuns}`, maxRuns);
        allGames.push(...games);
        summarize(games, diff);
        const li = a.lossInfo;
        if (li && li.kind === "loss") {
          console.log(
            `  ${String("").padEnd(12)} LOSS on run ${li.run}: moves=${li.moves} ` +
              `guesses=${li.guesses} time_ms=${li.time_ms}`,
          );
        } else {
          console.log(`  ${String("").padEnd(12)} no loss in ${maxRuns} replays (seed is strong)`);
        }
      } else if (args.seed !== null) {
        const count = args.multi > 0 ? args.multi : 1;
        const games = await a.request(`reqseed ${diff} ${args.seed} ${count}`, count);
        allGames.push(...games);
        summarize(games, diff, 1 < count && count <= 25);
      } else {
        const games = await a.request(`reqbatch ${diff} ${args.games}`, args.games);
        allGames.push(...games);
        summarize(games, diff);
      }
    }
    console.log(`total: ${allGames.length} requested games`);

    if (args.out) {
      const lines = [GAME_FIELDS.join(",")];
      for (const g of allGames) {
        lines.push([g.difficulty, String(g.seed), g.won, g.moves, g.time_ms, g.guesses].join(","));
      }
      fs.writeFileSync(args.out, lines.join("\n") + "\n", "utf8");
      console.log(`wrote ${allGames.length} rows to ${args.out}`);
    }
    return 0;
  } finally {
    a.close();
  }
}

function usage() {
  console.log(
    "usage: node ms/cli/ms_analyze.js [--host HOST] [--port PORT] " +
      "[--difficulty beginner|intermediate|expert] [--all] [--games N] " +
      "[--seed N] [--multi N] [--until-loss] [--out FILE] " +
      "[--solver-user USER] [--solver-pass PASS]",
  );
}

async function main() {
  const { values, positionals } = parseArgs({
    options: {
      host: { type: "string", default: "127.0.0.1" },
      port: { type: "string", default: "28571" },
      difficulty: { type: "string", default: "" },
      all: { type: "boolean", default: false },
      games: { type: "string", default: "200" },
      seed: { type: "string", default: "" },
      multi: { type: "string", default: "0" },
      "until-loss": { type: "boolean", default: false },
      out: { type: "string", default: "" },
      "solver-user": { type: "string", default: "" },
      "solver-pass": { type: "string", default: "" },
      help: { type: "boolean", default: false },
    },
    allowPositionals: true,
  });

  if (values.help || positionals.length) {
    usage();
    return values.help ? 0 : 2;
  }

  const args = {
    host: values.host,
    port: Number(values.port),
    difficulty: values.difficulty || null,
    all: values.all,
    games: Number(values.games),
    seed: values.seed !== "" ? BigInt(values.seed) : null,
    multi: Number(values.multi),
    untilLoss: values["until-loss"],
    out: values.out || null,
    solverUser: values["solver-user"] || null,
    solverPass: values["solver-pass"],
  };
  try {
    return await run(args);
  } catch (e) {
    console.error(`ms-analyze: ${e.message || e}`);
    return 1;
  }
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  process.exit(await main());
}

export { ConnectionError, summarize, main };
