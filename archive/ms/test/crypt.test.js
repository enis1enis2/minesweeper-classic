import { test } from "node:test";
import assert from "node:assert/strict";
import {
  generateKey,
  encrypt,
  decrypt,
  hashPassword,
  verifyPassword,
  safeEqual,
} from "../admin/crypt.js";

test("aes-gcm encrypt/decrypt roundtrip", () => {
  const key = generateKey();
  const blob = encrypt(key, Buffer.from('{"x": 1}'));
  assert.equal(decrypt(key, blob).toString(), '{"x": 1}');
});

test("aes-gcm rejects tampered blobs", () => {
  const key = generateKey();
  const blob = encrypt(key, Buffer.from("secret payload"));
  const tampered = Buffer.from(blob);
  tampered[tampered.length - 1] ^= 0xff;
  assert.throws(() => decrypt(key, tampered));
});

test("aes-gcm rejects wrong key", () => {
  const blob = encrypt(generateKey(), Buffer.from("x"));
  assert.throws(() => decrypt(generateKey(), blob));
});

test("scrypt hash/verify", () => {
  const h = hashPassword("correct horse battery staple xyz");
  assert.ok(verifyPassword(h, "correct horse battery staple xyz"));
  assert.ok(!verifyPassword(h, "wrong"));
  assert.ok(!verifyPassword(h, ""));
  assert.ok(!verifyPassword(h, null));
});

test("safeEqual compares strings in constant time", () => {
  assert.ok(safeEqual("admin", "admin"));
  assert.ok(!safeEqual("admin", "admim"));
  assert.ok(!safeEqual("", "a"));
  assert.ok(safeEqual(undefined, ""));
});
