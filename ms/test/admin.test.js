// admin.test.js - unit + end-to-end tests for the admin diagnostics stack
// (port of server/admin.py).  Covers DiagDB, AuthStore and the HTTP layer:
// ingest validation, at-rest encryption, login/lockout/sessions, viewer.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";
import { DiagDB, AuthStore } from "../admin/store.js";
import { createAdminServer } from "../admin/server.js";
import { generateKey, encrypt, decrypt, hashPassword } from "../admin/crypt.js";
import { totpValue, base32Encode } from "../admin/totp.js";

const PASSWORD = "correct horse battery staple xyz";

function makeConfigFile(dir, overrides = {}) {
  const cfg = {
    username: "admin",
    password_hash: hashPassword(PASSWORD),
    totp_secret_b32: base32Encode(crypto.randomBytes(20)),
    session_ttl_sec: 3600,
    ...overrides,
  };
  const p = path.join(dir, "admin.json");
  fs.writeFileSync(p, JSON.stringify(cfg));
  return { p, cfg };
}

function totpNow(secret) {
  return totpValue(secret, Math.floor(Date.now() / 1000 / 30));
}

// --------------------------------------------------------------------------

test("DiagDB insert / stats / recent rows", () => {
  const db = new DiagDB(":memory:");
  try {
    const now = Math.floor(Date.now() / 1000);
    db.insert(now, "1.2.3.4", Buffer.from("blob1"));
    db.insert(now, "1.2.3.4", Buffer.from("blob2"));
    db.insert(now, "5.6.7.8", Buffer.from("blob3"));
    const [total, recent] = db.stats();
    assert.equal(total, 3);
    assert.ok(recent >= 3);
    const rows = db.recentRows(2);
    assert.equal(rows.length, 2);
    assert.equal(rows[0][0], 3);
    assert.equal(rows[0][2], "5.6.7.8");
    assert.equal(rows[0][3].toString(), "blob3");
    assert.equal(rows[1][0], 2);
  } finally {
    db.close();
  }
});

test("AuthStore login, lockout and sessions", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "ms-auth-"));
  const { cfg } = makeConfigFile(dir);
  const auth = new AuthStore(path.join(dir, "admin.json"));
  const ip = "198.51.100.1";

  assert.deepEqual(
    auth.checkLogin(ip, "admin", PASSWORD, totpNow(cfg.totp_secret_b32)),
    [true, null]
  );
  assert.deepEqual(auth.checkLogin(ip, "admin", "wrong-password-here", "000000"), [
    false,
    "invalid credentials",
  ]);
  assert.deepEqual(auth.checkLogin(ip, "nobody", PASSWORD, "000000"), [
    false,
    "invalid credentials",
  ]);
  assert.deepEqual(auth.checkLogin(ip, "admin", PASSWORD, "000000"), [
    false,
    "invalid TOTP code",
  ]);

  // 5 failures lock the IP for 900s
  for (let i = 0; i < 5; i++) auth.recordFailure(ip);
  assert.ok(auth.lockedUntil(ip) > Math.floor(Date.now() / 1000));
  assert.equal(auth.checkLogin(ip, "admin", PASSWORD, "000000")[0], false);
  auth.clearFailures(ip);
  assert.equal(auth.lockedUntil(ip), 0);

  // sessions: issue -> validate -> revoke
  const [token, expiry] = auth.issueSession(ip);
  assert.ok(expiry > Math.floor(Date.now() / 1000));
  assert.equal(auth.validate(token), ip);
  assert.equal(auth.validate("bogus"), null);
  auth.revoke(token);
  assert.equal(auth.validate(token), null);

  const [t2] = auth.issueSession(ip);
  auth.revokeAll();
  assert.equal(auth.validate(t2), null);

  // expired sessions are pruned
  const auth2 = new AuthStore(makeConfigFile(dir, { session_ttl_sec: -1 }).p);
  const [t3] = auth2.issueSession(ip);
  assert.equal(auth2.validate(t3), null);
});

// --------------------------------------------------------------------------

test("admin HTTP end-to-end", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "ms-admin-e2e-"));
  const key = generateKey();
  const { cfg } = makeConfigFile(dir);
  const db = new DiagDB(":memory:");
  const auth = new AuthStore(path.join(dir, "admin.json"));
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
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const base = "http://127.0.0.1:" + server.address().port;

  const payload = {
    machine_id: "MACHINE-1",
    os: "Windows",
    cpu: "i7-12700K",
    cpu_cores: 12,
    gpu: "RTX 3080",
    ram_mb: 32768,
    display: "1920x1080",
    game_version: "1.2.3",
    uptime_sec: 86400,
    crash_text: null,
  };

  try {
    // healthz
    const hz = await fetch(base + "/ms-admin/healthz");
    assert.equal(hz.status, 200);
    assert.equal(await hz.text(), "ok\n");

    // ingest: valid payload
    const ok = await fetch(base + "/ms-diag/ingest", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(payload),
    });
    assert.equal(ok.status, 200);
    assert.deepEqual(await ok.json(), { ok: true });

    // stored blob decrypts back to the (sorted-key) payload
    const rows = db.recentRows(1);
    assert.equal(rows.length, 1);
    const stored = JSON.parse(decrypt(key, rows[0][3]).toString("utf8"));
    assert.equal(stored.machine_id, "MACHINE-1");
    assert.equal(stored.crash_text, null);

    // ingest: validation failures
    const missing = JSON.parse(JSON.stringify(payload));
    delete missing.cpu_cores;
    assert.equal(
      (await fetch(base + "/ms-diag/ingest", { method: "POST", body: JSON.stringify(missing) })).status,
      400
    );
    assert.equal(
      (await fetch(base + "/ms-diag/ingest", { method: "POST", body: "{not json" })).status,
      400
    );
    assert.equal(
      (await fetch(base + "/ms-diag/ingest", { method: "POST", body: "[1,2,3]" })).status,
      400
    );
    const badType = { ...payload, ram_mb: "lots" };
    assert.equal(
      (await fetch(base + "/ms-diag/ingest", { method: "POST", body: JSON.stringify(badType) })).status,
      400
    );
    assert.equal(
      (await fetch(base + "/ms-diag/ingest", { method: "POST", body: "" })).status,
      400
    );
    assert.equal(
      (await fetch(base + "/ms-diag/ingest", { method: "POST", body: Buffer.alloc(70000) })).status,
      413
    );

    // viewer requires a session
    let res = await fetch(base + "/ms-admin/");
    assert.equal(res.status, 401);
    assert.match(await res.text(), /Sign in/);

    // failed logins do not grant access
    for (const creds of [
      { username: "admin", password: "wrong-password-here", totp: "000000" },
      { username: "nobody", password: PASSWORD, totp: "000000" },
      { username: "admin", password: PASSWORD, totp: "000000" },
      { username: "", password: "", totp: "" },
    ]) {
      res = await fetch(base + "/ms-admin/login", {
        method: "POST",
        body: new URLSearchParams(creds),
        redirect: "manual",
      });
      assert.equal(res.status, 401);
    }

    // correct login
    res = await fetch(base + "/ms-admin/login", {
      method: "POST",
      body: new URLSearchParams({
        username: "admin",
        password: PASSWORD,
        totp: totpNow(cfg.totp_secret_b32),
      }),
      redirect: "manual",
    });
    assert.equal(res.status, 302);
    const setCookie = res.headers.get("set-cookie");
    assert.match(setCookie, /^ms_admin=/);
    const token = setCookie.split(";")[0].split("=")[1];

    // viewer shows the decrypted row
    res = await fetch(base + "/ms-admin/", {
      headers: { cookie: "ms_admin=" + token },
    });
    assert.equal(res.status, 200);
    const viewer = await res.text();
    assert.match(viewer, /Minesweeper diagnostics/);
    assert.match(viewer, /MACHINE-1/);
    assert.match(viewer, /i7-12700K/);
    assert.match(viewer, /signed in from/);

    // unknown admin paths 404 with a valid session
    res = await fetch(base + "/ms-admin/nope", {
      headers: { cookie: "ms_admin=" + token },
    });
    assert.equal(res.status, 404);

    // logout clears the session
    res = await fetch(base + "/ms-admin/logout", {
      method: "POST",
      headers: { cookie: "ms_admin=" + token },
      redirect: "manual",
    });
    assert.equal(res.status, 302);
    res = await fetch(base + "/ms-admin/", {
      headers: { cookie: "ms_admin=" + token },
    });
    assert.equal(res.status, 401);

    // revoke-all kills every session
    const [t2] = auth.issueSession("127.0.0.1");
    res = await fetch(base + "/ms-admin/revoke-all", {
      method: "POST",
      headers: { cookie: "ms_admin=" + t2 },
      redirect: "manual",
    });
    assert.equal(res.status, 302);
    res = await fetch(base + "/ms-admin/", {
      headers: { cookie: "ms_admin=" + t2 },
    });
    assert.equal(res.status, 401);

    // lockout: 5 failed attempts, then the account is locked
    for (let i = 0; i < 5; i++) {
      res = await fetch(base + "/ms-admin/login", {
        method: "POST",
        body: new URLSearchParams({ username: "admin", password: "wrong-x", totp: "000000" }),
        redirect: "manual",
      });
      assert.equal(res.status, 401);
    }
    res = await fetch(base + "/ms-admin/login", {
      method: "POST",
      body: new URLSearchParams({ username: "admin", password: PASSWORD, totp: "000000" }),
      redirect: "manual",
    });
    assert.equal(res.status, 423);
    assert.match(await res.text(), /locked out|Retry/);

    // server still alive after the lockout storm
    assert.equal((await fetch(base + "/ms-admin/healthz")).status, 200);
  } finally {
    server.close();
    db.close();
  }
});
