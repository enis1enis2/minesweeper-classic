import { test } from "node:test";
import assert from "node:assert/strict";
import crypto from "node:crypto";
import { totpValue, totpVerify, base32Encode, base32Decode } from "../admin/totp.js";

const SECRET = base32Encode(Buffer.from("12345678901234567890", "ascii"));

test("RFC 6238 SHA1 test vectors (8-digit)", () => {
  assert.equal(totpValue(SECRET, 1, 8), "94287082");
  assert.equal(totpValue(SECRET, 666666666, 8), "65353130");
});

test("6-digit code derives from 8-digit vector", () => {
  assert.equal(totpValue(SECRET, 1, 6), "287082");
});

test("totpVerify checks the current 30s step with a window", () => {
  assert.ok(totpVerify(SECRET, "287082", { window: 0, ts: 59 }));
  assert.ok(!totpVerify(SECRET, "000000", { window: 0, ts: 59 }));
  assert.ok(!totpVerify(SECRET, "287082", { window: 0, ts: 29 })); // previous step
  const now = Math.floor(Date.now() / 1000);
  const live = totpValue(SECRET, Math.floor(now / 30));
  assert.ok(totpVerify(SECRET, live));
  assert.ok(totpVerify(SECRET, totpValue(SECRET, Math.floor(now / 30) + 1)));
  assert.ok(!totpVerify(SECRET, "12345")); // wrong length
  assert.ok(!totpVerify(SECRET, "abcdef")); // non-digits
});

test("base32 encode/decode roundtrip", () => {
  const raw = Buffer.from("hello world", "utf8");
  assert.ok(base32Decode(base32Encode(raw)).equals(raw));
  const secret = base32Encode(crypto.randomBytes(20));
  assert.equal(secret.length, 32);
  assert.equal(base32Decode(secret).length, 20);
});
