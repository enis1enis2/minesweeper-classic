// helpers/e2e.js - wire-protocol helpers for the sim-server E2E test.
// Port of server/selfcheck.py's LineReader / auth / request helpers.

import net from "node:net";
import crypto from "node:crypto";
import readline from "node:readline";
import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
export const MS_DIR = path.dirname(path.dirname(here)); // ms/

export class LineReader {
  // Robust line reader backed by node:readline's async iterator (handles
  // arbitrary TCP chunk boundaries; throws on connection close).
  constructor(sock) {
    this.sock = sock;
    this.rl = readline.createInterface({ input: sock, crlfDelay: Infinity });
    this._it = this.rl[Symbol.asyncIterator]();
  }

  async readLine() {
    const { value, done } = await this._it.next();
    if (done || value === undefined) {
      throw new Error("connection closed");
    }
    return value.replace(/\r$/, "").trim();
  }

  close() {
    this.rl.close();
  }
}

export async function connect(port) {
  for (let i = 0; i < 100; i++) {
    try {
      return await new Promise((resolve, reject) => {
        const s = net.connect(port, "127.0.0.1", () => {
          s.removeListener("error", reject);
          resolve(s);
        });
        s.once("error", reject);
      });
    } catch {
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  throw new Error("server did not open port " + port);
}

export async function spawnServer(port, db, extra = []) {
  // Spawn the sim server on --port 0 (OS-assigned) and read the real bound
  // port back from its banner line, so parallel/rerun test runs never collide
  // on a fixed port.
  const proc = spawn(
    process.execPath,
    [
      path.join(MS_DIR, "sim-server", "server.js"),
      "--host",
      "127.0.0.1",
      "--port",
      String(port),
      "--db",
      db,
      "--rate",
      "50",
      "--seed",
      "99",
      ...extra,
    ],
    { stdio: ["ignore", "pipe", "pipe"] }
  );
  const errors = [];
  proc.stderr.on("data", (d) => errors.push(d.toString()));
  const banner = await waitForBanner(proc);
  if (!banner) {
    proc.kill();
    throw new Error("server failed to start: " + errors.join(""));
  }
  const m = /listening on [^:]+:(\d+)/.exec(banner);
  if (!m) throw new Error("could not parse bound port from: " + banner);
  const boundPort = Number(m[1]);
  const sock = await connect(boundPort);
  return { proc, sock, port: boundPort, stdoutLines: [], stderr: errors };
}

function waitForBanner(proc) {
  return new Promise((resolve) => {
    let out = "";
    const timer = setTimeout(() => resolve(null), 15000);
    proc.stdout.on("data", (d) => {
      out += d.toString();
      if (out.indexOf("listening") >= 0) {
        clearTimeout(timer);
        resolve(out);
      }
    });
    proc.on("exit", () => {
      clearTimeout(timer);
      resolve(null);
    });
  });
}

export async function drainBroadcast(reader) {
  let seenSeed = null;
  let seenOutcome = null;
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    const parts = (await reader.readLine()).split(/\s+/);
    if (!parts.length) continue;
    if (parts[0] === "seed" && seenSeed === null) seenSeed = parts;
    else if (parts[0] === "outcome") {
      seenOutcome = parts;
      if (
        seenSeed !== null &&
        parts[1] === seenSeed[1] &&
        parts[2] === seenSeed[2]
      ) {
        break;
      }
    }
  }
  if (seenSeed === null || seenOutcome === null) {
    throw new Error("no seed/outcome pair seen");
  }
  if (seenOutcome[1] !== seenSeed[1] || seenOutcome[2] !== seenSeed[2]) {
    throw new Error("outcome seed/diff mismatch");
  }
  if (seenOutcome.length !== 7) throw new Error("malformed outcome line");
  return [seenSeed, seenOutcome];
}

export function digestHex(password, nonce) {
  return crypto
    .createHmac("sha256", password)
    .update("ms-auth:" + nonce)
    .digest("hex");
}

export async function authHandshake(sock, reader, user, password) {
  sock.write("auth " + user + "\n");
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline) {
    const parts = (await reader.readLine()).split(/\s+/);
    if (!parts.length) continue;
    if (parts[0] === "authchal") {
      sock.write("authresp " + digestHex(password, parts[1]) + "\n");
    } else if (parts[0] === "authok") return true;
    else if (parts[0] === "autherr") return false;
  }
  throw new Error("timed out during auth handshake");
}

export async function expect(reader, prefix, what) {
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline) {
    const parts = (await reader.readLine()).split(/\s+/);
    if (parts.length && parts[0] === prefix) return parts;
  }
  throw new Error("no " + what + " line seen");
}

export async function expectAny(reader, prefixes, what) {
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline) {
    const parts = (await reader.readLine()).split(/\s+/);
    if (parts.length && prefixes.includes(parts[0])) return parts;
  }
  throw new Error("no " + what + " line seen");
}

export async function collectRequests(reader, sock, reqLine) {
  const requested = [];
  sock.write(reqLine + "\n");
  let pending = null;
  let done = null;
  const deadline = Date.now() + 15000;
  while (Date.now() < deadline) {
    const parts = (await reader.readLine()).split(/\s+/);
    if (!parts.length) continue;
    if (parts[0] === "reqgame") pending = [parts[1], parts[2]];
    else if (parts[0] === "outcome" && pending !== null) {
      if (parts[1] === pending[0] && parts[2] === pending[1]) {
        requested.push([parts[1], parts[2], parts[3], parts[4], parts[5], parts[6]]);
        pending = null;
      }
    } else if (parts[0] === "reqdone") {
      done = [parts[1], parts[2]];
      break;
    }
  }
  return [done, requested];
}

export async function stopProc(proc) {
  proc.kill();
  await new Promise((r) => setTimeout(r, 300));
}
