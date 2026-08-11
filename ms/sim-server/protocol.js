// protocol.js - wire-protocol handlers (ports of the ms_server.py connection
// thread, auth / leaderboard / request handlers, the producer and the game
// runner).

import crypto from "node:crypto";
import { SimBoard, SimClient } from "../core/sim-engine.js";
import { Random } from "../core/mt19937.js";
import {
  DIFFS,
  GAME_CPU_SECONDS,
  HEAVY_CPU_SECONDS,
  MAX_AUTH_FAILS,
  MAX_LINE,
  LB_WINDOW,
  LB_MAX,
  LB_MAX_IPS,
  NAME_RE,
} from "./config.js";

export { DIFFS, GAME_CPU_SECONDS, HEAVY_CPU_SECONDS };

const nowSec = () => Math.floor(Date.now() / 1000);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Python int("...") is base-10 decimal only: no scientific notation, no hex,
// no underscores.  Node Number("1e3") and BigInt("0x10") accept those, so
// tokens are validated against the same grammar before conversion.
const INT_RE = /^[+-]?[0-9]+$/;

function parseSeedToken(tok) {
  if (!INT_RE.test(tok)) return null;
  try {
    return BigInt(tok);
  } catch {
    return null;
  }
}

function parseCountToken(tok) {
  if (!INT_RE.test(tok)) return null;
  const n = Number(tok);
  return Number.isInteger(n) ? n : null;
}

// hmac.compare_digest() for strings: constant-time, rejects on length
// mismatch without leaking content timing.
function timingSafeEqualStr(a, b) {
  const ba = Buffer.from(a, "utf8");
  const bb = Buffer.from(b, "utf8");
  if (ba.length !== bb.length) return false;
  return crypto.timingSafeEqual(ba, bb);
}

export function outcomeLine(diff, seed, g) {
  return (
    "outcome " +
    diff +
    " " +
    seed +
    " " +
    (g.won ? 1 : 0) +
    " " +
    g.moves +
    " " +
    g.time_ms +
    " " +
    g.guesses
  );
}

export function handleConn(server, conn) {
  const addrS = `${conn.remoteAddress}:${conn.remotePort}`;
  server.hub.add(addrS, conn);
  let buf = "";
  conn.on("data", (chunk) => {
    // port of raw.decode("ascii", "replace"): bytes >= 0x80 become U+FFFD
    buf += chunk.toString("latin1").replace(/[\x80-\xFF]/g, "\uFFFD");
    if (buf.length > MAX_LINE && buf.indexOf("\n") < 0) {
      console.log(`  conn: ${addrS} oversized line, closing`);
      conn.destroy();
      return;
    }
    let nl;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const raw = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      const text = raw.replace(/\r$/, "").trim();
      if (!text) continue;
      if (text.startsWith("metric ")) {
        server.db.record_metric(nowSec(), addrS, text);
      } else if (text.startsWith("auth ")) {
        handleAuth(server, addrS, conn, text);
      } else if (text.startsWith("authresp ")) {
        handleAuthresp(server, addrS, conn, text);
      } else if (text.startsWith("lbscore ")) {
        handleLbscore(server, addrS, text);
      } else if (text.startsWith("lbtop ")) {
        handleLbtop(server, addrS, text);
      } else if (
        text.startsWith("reqseed ") ||
        text.startsWith("reqbatch ") ||
        text.startsWith("requntil ")
      ) {
        if (!server.solverEnabled || !server.hub.is_authed(addrS)) {
          server.hub.send_to(addrS, "reqdenied");
        } else {
          server.reqWorkers.enqueue(addrS, text);
        }
      }
    }
  });
  conn.on("error", () => {});
  conn.on("close", () => {
    server.hub.remove(addrS);
    server.reqWorkers.drop(addrS);
  });
}

export function handleAuth(server, addrS, conn, line) {
  const toks = line.split(/\s+/);
  if (toks.length < 2 || !server.solverEnabled) {
    server.hub.send_to(addrS, "autherr");
    return;
  }
  const user = toks[1];
  if (!timingSafeEqualStr(user, server.solverUser)) {
    server.hub.send_to(addrS, "autherr");
    console.log(`  auth: unknown user ${JSON.stringify(user)} from ${addrS}`);
    return;
  }
  const nonce = server.hub.auth_begin(addrS, user);
  if (nonce === null) {
    server.hub.send_to(addrS, "autherr");
    return;
  }
  server.hub.send_to(addrS, "authchal " + nonce);
}

export function handleAuthresp(server, addrS, conn, line) {
  const toks = line.split(/\s+/);
  if (toks.length < 2) {
    server.hub.send_to(addrS, "autherr");
    return;
  }
  const cl = server.hub.get(addrS);
  const nonce = cl ? cl.nonce : null;
  const user = cl ? cl.user : null;
  if (nonce === null) {
    server.hub.send_to(addrS, "autherr");
    return;
  }
  const expected = crypto
    .createHmac("sha256", server.solverPass)
    .update("ms-auth:" + nonce)
    .digest("hex");
  const [ok, fails] = server.hub.auth_resolve(addrS, toks[1], expected);
  if (ok) {
    server.hub.send_to(addrS, "authok");
    console.log(`  auth: ok user=${user} from ${addrS}`);
  } else {
    server.hub.send_to(addrS, "autherr");
    console.log(`  auth: FAILED user=${user} from ${addrS} (fails=${fails})`);
    if (fails >= MAX_AUTH_FAILS) {
      // lockout: drop the connection (caught by conn close handler)
      conn.destroy();
    }
  }
}

export function handleLbscore(server, addrS, line) {
  const toks = line.split(/\s+/);
  if (toks.length !== 4) return;
  const name = toks[1];
  const diff = toks[2].toLowerCase();
  if (!NAME_RE.test(name) || !DIFFS.includes(diff)) return;
  if (parseCountToken(toks[3]) === null) return;
  const ms = Number(toks[3]);
  if (ms < 0 || ms > 3600000) return;
  const ip = addrS.slice(0, addrS.lastIndexOf(":"));
  // time.monotonic(): wall time can jump; the rate limit window must not.
  const now = performance.now() / 1000;
  const hist = (server.lbHist[ip] = (server.lbHist[ip] || []).filter(
    (t) => now - t < LB_WINDOW
  ));
  if (hist.length >= LB_MAX) {
    server.hub.send_to(addrS, "lbdenied");
    return;
  }
  hist.push(now);
  if (hist.length > LB_MAX) {
    server.hub.send_to(addrS, "lbdenied");
    return;
  }
  if (Object.keys(server.lbHist).length > LB_MAX_IPS) {
    for (const k of Object.keys(server.lbHist)) {
      if (!server.lbHist[k].some((t) => now - t < LB_WINDOW)) {
        delete server.lbHist[k];
      }
    }
  }
  while (Object.keys(server.lbHist).length > LB_MAX_IPS) {
    const k = Object.keys(server.lbHist).reduce((a, b) =>
      server.lbHist[a][server.lbHist[a].length - 1] <=
      server.lbHist[b][server.lbHist[b].length - 1]
        ? a
        : b
    );
    delete server.lbHist[k];
  }
  const [improved, rank] = server.db.record_score(name, diff, ms);
  if (improved) {
    server.hub.send_to(
      addrS,
      "lbstored " + rank + " " + diff + " " + name + " " + ms
    );
  } else {
    server.hub.send_to(addrS, "lbnotop");
  }
}

export function handleLbtop(server, addrS, line) {
  const toks = line.split(/\s+/);
  let count = 10;
  let diff = null;
  if (toks.length >= 3 && DIFFS.includes(toks[1].toLowerCase())) {
    diff = toks[1].toLowerCase();
    count = parseCountToken(toks[2]);
    if (count === null) return;
  } else if (toks.length >= 2) {
    count = parseCountToken(toks[1]);
    if (count === null) return;
  }
  if (count < 1 || count > 100) return;
  const entries = server.db.top_scores(diff, count);
  if (diff === null) {
    server.hub.send_to(addrS, "lbtop " + entries.length);
  } else {
    server.hub.send_to(addrS, "lbtop " + diff + " " + entries.length);
  }
  for (const [rank, name, d, ms, ts] of entries) {
    server.hub.send_to(
      addrS,
      "lbentry " + rank + " " + d + " " + name + " " + ms + " " + ts
    );
  }
  server.hub.send_to(addrS, "lbdone");
}

// Play one simulated game in a worker and record it.  Returns the game dict.
// `opts` carries either { rngState } (broadcast: decision RNG continues the
// producer's stream, returned as `_rngState`) or { decisionSeed } (requested:
// fresh Random(seed ^ run<<32), no state to carry), plus `requester`.
export async function simulateGame(server, diff, seed, opts = {}) {
  const task = { diff, seed };
  if (opts.rngState) task.rngState = opts.rngState;
  if (opts.decisionSeed !== undefined) task.decisionSeed = opts.decisionSeed;
  const msg = await server.pool.submit(task);
  const g = msg.g;
  g.requester = opts.requester ?? null;
  server.db.record_game(g);
  return { g, rngState: msg.rngState };
}

export async function handleRequest(server, addrS, line) {
  if (!server.solverEnabled || !server.hub.is_authed(addrS)) {
    server.hub.send_to(addrS, "reqdenied");
    return;
  }
  const hub = server.hub;
  const toks = line.split(/\s+/);
  const cmd = toks[0].toLowerCase();
  let diff = null;
  let seed = null;
  let count = 1;
  let until = false;
  let seedRequired = false;
  try {
    if (cmd === "reqseed" && toks.length >= 3) {
      diff = toks[1].toLowerCase();
      seed = parseSeedToken(toks[2]);
      seedRequired = true;
      if (toks.length >= 4) count = parseCountToken(toks[3]);
    } else if (cmd === "reqbatch" && toks.length >= 3) {
      diff = toks[1].toLowerCase();
      count = parseCountToken(toks[2]);
    } else if (cmd === "requntil" && toks.length >= 3) {
      diff = toks[1].toLowerCase();
      until = true;
      seed = parseSeedToken(toks[2]);
      seedRequired = true;
      if (toks.length >= 4) count = parseCountToken(toks[3]);
    } else {
      return;
    }
  } catch {
    return;
  }
  if (seedRequired && seed === null) return;
  if (count === null) return;
  if (!DIFFS.includes(diff) || !Number.isInteger(count) || count < 1) return;
  count = Math.min(count, server.maxRequest);

  const heavy = count * (GAME_CPU_SECONDS[diff] || 0) >= HEAVY_CPU_SECONDS;
  if (heavy) {
    hub.send_to(addrS, "reqwait " + diff + " " + (seed !== null ? seed : count));
    await server.gate.acquire();
  }
  try {
    const batchRng = seed !== null ? new Random(seed) : new Random(null);
    let played = 0;
    let loss = null;
    for (let run = 0; run < count; run++) {
      let s = seed;
      if (s === null) s = batchRng.randrange(0n, 1n << 63n);
      const decisionSeed = s ^ (BigInt(run) << 32n);
      if (!hub.send_to(addrS, "reqgame " + diff + " " + s)) break;
      const { g } = await simulateGame(server, diff, s, {
        requester: addrS,
        decisionSeed,
      });
      if (!hub.send_to(addrS, "seed " + diff + " " + s)) break;
      if (!hub.send_to(addrS, outcomeLine(diff, s, g))) break;
      played += 1;
      if (until && !g.won) {
        // won is serialized as 0/1 like Python's int(g["won"])
        loss = [g.won ? 1 : 0, g.moves, g.time_ms, g.guesses];
        break;
      }
    }
    if (until) {
      if (loss !== null) {
        hub.send_to(
          addrS,
          "lossfound " +
            diff +
            " " +
            seed +
            " " +
            (played - 1) +
            " " +
            loss[0] +
            " " +
            loss[1] +
            " " +
            loss[2] +
            " " +
            loss[3]
        );
      } else {
        hub.send_to(addrS, "noloss " + diff + " " + seed + " " + played);
      }
    }
    hub.send_to(addrS, "reqdone " + diff + " " + played);
  } finally {
    if (heavy) server.gate.release();
  }
}

export async function produce(server, rng) {
  while (!server.stop) {
    while (server.hub.count() === 0 && !server.stop) {
      await sleep(250);
    }
    if (server.stop) break;

    const diff = rng.choice(server.diffs);
    const seed = rng.randrange(0n, 1n << 63n);
    const { g, rngState } = await simulateGame(server, diff, seed, {
      rngState: rng.snapshot(),
    });
    rng.restore(rngState);

    server.hub.broadcast("seed " + diff + " " + seed);
    server.hub.broadcast(outcomeLine(diff, seed, g));

    if (server.rate > 0) {
      await sleep(1000 / server.rate);
    }
  }
}
