// store.js - device_diagnostics persistence + single-admin auth/session store.
// Zero-dependency port of admin.py's DiagDB and AuthStore (node:sqlite vs
// sqlite3, in-memory Map sessions instead of dicts, everything single-threaded).

import fs from "node:fs";
import path from "node:path";
import crypto from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import { safeEqual, verifyPassword } from "./crypt.js";
import { totpVerify } from "./totp.js";

export const SCHEMA = `
CREATE TABLE IF NOT EXISTS device_diagnostics(
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    addr TEXT NOT NULL,
    blob BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_diag_ts ON device_diagnostics(ts);
`;

const nowSec = () => Math.floor(Date.now() / 1000);

export class DiagDB {
  constructor(dbPath) {
    if (dbPath !== ":memory:") {
      const dir = path.dirname(dbPath);
      if (dir) fs.mkdirSync(dir, { recursive: true });
    }
    this.conn = new DatabaseSync(dbPath);
    this.conn.exec("PRAGMA journal_mode=WAL");
    this.conn.exec("PRAGMA synchronous=NORMAL");
    this.conn.exec("PRAGMA busy_timeout=5000");
    this.conn.exec(SCHEMA);
  }

  insert(ts, addr, blob) {
    this.conn
      .prepare("INSERT INTO device_diagnostics(ts, addr, blob) VALUES(?,?,?)")
      .run(ts, addr, blob);
  }

  stats() {
    const total = Number(
      this.conn
        .prepare("SELECT COUNT(*) AS n FROM device_diagnostics")
        .get().n
    );
    const recent = Number(
      this.conn
        .prepare("SELECT COUNT(*) AS n FROM device_diagnostics WHERE ts>=?")
        .get(nowSec() - 86400).n
    );
    return [total, recent];
  }

  recentRows(limit = 200) {
    return this.conn
      .prepare(
        "SELECT id, ts, addr, blob FROM device_diagnostics ORDER BY id DESC LIMIT ?"
      )
      .all(limit)
      .map((r) => [r.id, r.ts, r.addr, Buffer.from(r.blob)]);
  }

  close() {
    this.conn.close();
  }
}

export class AuthStore {
  constructor(path) {
    const cfg = JSON.parse(fs.readFileSync(path, "utf8"));
    this.username = cfg.username;
    this.passwordHash = cfg.password_hash;
    this.totpSecret = cfg.totp_secret_b32;
    this.ttl = Number(cfg.session_ttl_sec ?? 4 * 3600);
    this.sessions = new Map(); // token -> [expiry_ts, ip]
    this.epoch = 0;
    this.failures = new Map(); // ip -> [ts, ...]
  }

  _prune(now) {
    for (const [token, [exp]] of this.sessions) {
      if (exp <= now) this.sessions.delete(token);
    }
    for (const [ip, fs] of this.failures) {
      if (!fs.length || fs[fs.length - 1] < now - 900) {
        this.failures.delete(ip);
      }
    }
  }

  lockedUntil(ip, now) {
    now = now ?? nowSec();
    const fs = this.failures.get(ip) ?? [];
    if (fs.length >= 5 && fs[fs.length - 1] >= now - 900) {
      return fs[fs.length - 1] + 900;
    }
    return 0;
  }

  recordFailure(ip) {
    const now = nowSec();
    const fs = this.failures.get(ip) ?? [];
    fs.push(now);
    this.failures.set(ip, fs.filter((t) => t >= now - 900));
  }

  clearFailures(ip) {
    this.failures.delete(ip);
  }

  checkLogin(ip, username, password, code, now) {
    if (this.lockedUntil(ip, now)) {
      return [false, "too many failed attempts (locked out)"];
    }
    if (!safeEqual(username ?? "", this.username)) {
      return [false, "invalid credentials"];
    }
    if (!verifyPassword(this.passwordHash, password ?? "")) {
      return [false, "invalid credentials"];
    }
    if (!totpVerify(this.totpSecret, code ?? "")) {
      return [false, "invalid TOTP code"];
    }
    this.clearFailures(ip);
    return [true, null];
  }

  issueSession(ip) {
    const now = nowSec();
    const token = crypto.randomBytes(32).toString("base64url");
    this._prune(now);
    this.sessions.set(token, [now + this.ttl, ip]);
    return [token, now + this.ttl];
  }

  validate(token) {
    if (!token) return null;
    const now = nowSec();
    this._prune(now);
    const s = this.sessions.get(token);
    if (s === undefined) return null;
    if (s[0] <= now) {
      this.sessions.delete(token);
      return null;
    }
    return s[1]; // the ip this session was issued to
  }

  revoke(token) {
    this.sessions.delete(token);
  }

  revokeAll() {
    this.sessions.clear();
    this.epoch += 1;
  }
}
