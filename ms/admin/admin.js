// admin.js - diagnostics ingest + admin viewer (CLI entry).
//
// Zero-dependency port of server/admin.py.  The desktop client POSTs device
// diagnostics to /ms-diag/ingest; every payload is encrypted at rest (AES-GCM,
// key file, 0400).  A read-only /ms-admin/ viewer shows the decrypted rows to
// one administrator (username + scrypt password + TOTP, per-IP lockout,
// short-lived server-side sessions).
//
// Run:
//   node admin/admin.js --init
//   node admin/admin.js [--host 127.0.0.1] [--port 8444]
//       [--db data/diag.db] [--config data/admin.json] [--key data/diag.key]
//       [--session-ttl 14400]
//   node admin/admin.js --selfcheck

import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import readline from "node:readline/promises";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { generateKey, encrypt, decrypt, hashPassword, verifyPassword } from "./crypt.js";
import { totpValue, totpVerify, base32Encode, otpauthUri } from "./totp.js";
import { DiagDB, AuthStore } from "./store.js";
import { createAdminServer } from "./server.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function cmdInit(args) {
  const keyDir = path.dirname(args.key);
  if (keyDir) fs.mkdirSync(keyDir, { recursive: true });
  const cfgDir = path.dirname(args.config);
  if (cfgDir) fs.mkdirSync(cfgDir, { recursive: true });

  let key;
  if (fs.existsSync(args.key) && fs.statSync(args.key).size > 0) {
    key = fs.readFileSync(args.key, "utf8");
    console.log("key file exists, reusing it (" + args.key + ")");
  } else {
    key = generateKey();
    const fd = fs.openSync(args.key, "w", 0o400);
    try {
      fs.writeSync(fd, key);
    } finally {
      fs.closeSync(fd);
    }
    console.log("wrote new key: " + args.key + " (mode 0400)");
  }

  return askPassword().then((pw) => {
    const pwHash = hashPassword(pw);
    const secret = base32Encode(crypto.randomBytes(20));
    const cfg = {
      username: args.username,
      password_hash: pwHash,
      totp_secret_b32: secret,
      session_ttl_sec: args.sessionTtl,
    };
    const fd = fs.openSync(args.config, "w", 0o600);
    try {
      fs.writeSync(fd, JSON.stringify(cfg, null, 2));
    } finally {
      fs.closeSync(fd);
    }
    console.log("wrote config: " + args.config + " (mode 0600)");
    console.log();
    console.log("Add TOTP to your authenticator app:");
    console.log("  " + otpauthUri(args.username, "Minesweeper Admin", secret));
    console.log();
    console.log("Verify it works, then start the service:");
    console.log("  systemctl start minesweeper-admin");
    return 0;
  });
}

async function askPassword() {
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
  const lines = rl[Symbol.asyncIterator]();
  const promptLine = async (prompt) => {
    process.stdout.write(prompt);
    const { value } = await lines.next();
    return String(value ?? "").replace(/^\uFEFF/, ""); // strip a UTF-8 BOM
  };
  try {
    for (;;) {
      const pw = await promptLine("Password (>= 20 chars): ");
      const pw2 = await promptLine("Repeat password: ");
      if (pw.length < 20) {
        console.log("password too short (min 20 characters)");
        continue;
      }
      if (pw !== pw2) {
        console.log("passwords do not match");
        continue;
      }
      return pw;
    }
  } finally {
    rl.close();
  }
}

function cmdSelfcheck() {
  let ok = true;
  const check = (name, cond) => {
    console.log("  " + String(name).padEnd(34) + " " + (cond ? "OK" : "FAIL"));
    if (!cond) ok = false;
  };

  const b32 = base32Encode(Buffer.from("12345678901234567890", "ascii"));
  check("totp counter 1 (8d) -> 94287082", totpValue(b32, 1, 8) === "94287082");
  check("totp counter 666666666 (8d) -> 65353130", totpValue(b32, 666666666, 8) === "65353130");
  check("totp 6-digit derives from 8-digit vector", totpValue(b32, 1, 6) === "287082");
  check("totp current step (ts=59) contains vector", totpVerify(b32, "287082", { window: 0, ts: 59 }));
  check("totp rejects bad code", !totpVerify(b32, "000000", { window: 0, ts: 59 }));

  const fk = generateKey();
  const blob = encrypt(fk, Buffer.from('{"x": 1}'));
  check("aes-gcm encrypt/decrypt roundtrip", decrypt(fk, blob).toString() === '{"x": 1}');
  const tampered = Buffer.from(blob);
  tampered[tampered.length - 1] ^= 0xff;
  let tamperRejected = false;
  try {
    decrypt(fk, tampered);
  } catch {
    tamperRejected = true;
  }
  check("aes-gcm rejects tampered blob", tamperRejected);

  const h = hashPassword("correct horse battery staple xyz");
  check("scrypt hash/verify", verifyPassword(h, "correct horse battery staple xyz"));
  check("scrypt rejects wrong password", !verifyPassword(h, "wrong"));

  console.log("selfcheck:", ok ? "PASS" : "FAIL");
  return ok ? 0 : 1;
}

async function run(args) {
  let key = null;
  try {
    key = fs.readFileSync(args.key, "utf8").trim();
  } catch {
    key = null;
  }
  if (!key) {
    console.error("no key at " + args.key + " -- run --init first");
    return 2;
  }
  let db, auth;
  try {
    db = new DiagDB(args.db);
    auth = new AuthStore(args.config);
  } catch (e) {
    console.error("cannot load state: " + e.message);
    return 2;
  }

  const state = {
    db,
    auth,
    fernet: {
      encrypt: (buf) => encrypt(key, buf),
      decrypt: (blob) => decrypt(key, blob),
    },
    ingestCounts: [],
  };
  const server = createAdminServer(state);
  try {
    await new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(args.port, args.host, () => {
        server.removeListener("error", reject);
        resolve();
      });
    });
  } catch (e) {
    console.error("cannot bind " + args.host + ":" + args.port + ": " + e.message);
    db.close();
    return 2;
  }

  const boundPort = server.address().port;
  console.log(
    "ms-admin listening on " + args.host + ":" + boundPort +
    "  (db=" + args.db + " config=" + args.config + ")"
  );

  let stopping = false;
  const shutdown = () => {
    if (stopping) return;
    stopping = true;
    console.log("\nshutting down...");
    server.close();
    db.close();
  };
  process.on("SIGINT", () => void shutdown());
  process.on("SIGTERM", () => void shutdown());

  while (!stopping) {
    await sleep(250);
  }
  return 0;
}

function usage() {
  console.log(
    "usage: node admin/admin.js [--host HOST] [--port PORT] [--db FILE] " +
      "[--config FILE] [--key FILE] [--session-ttl SEC] [--init] " +
      "[--username USER] [--selfcheck]"
  );
}

async function main() {
  const { values, positionals } = parseArgs({
    options: {
      host: { type: "string", default: "127.0.0.1" },
      port: { type: "string", default: "8444" },
      db: { type: "string", default: path.join(HERE, "data", "diag.db") },
      config: { type: "string", default: path.join(HERE, "data", "admin.json") },
      key: { type: "string", default: path.join(HERE, "data", "diag.key") },
      "session-ttl": { type: "string", default: "14400" },
      init: { type: "boolean", default: false },
      username: { type: "string", default: "admin" },
      selfcheck: { type: "boolean", default: false },
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
    db: values.db,
    config: values.config,
    key: values.key,
    sessionTtl: Number(values["session-ttl"]),
    username: values.username,
  };

  if (values.init) return await cmdInit(args);
  if (values.selfcheck) return cmdSelfcheck();
  return run(args);
}

const isMain =
  process.argv[1] !== undefined &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  process.exit(await main());
}

export { run, main, cmdInit, cmdSelfcheck };
