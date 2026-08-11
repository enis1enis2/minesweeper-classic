// crypt.js - authenticated encryption at rest + password hashing.
//
// Node equivalents of admin.py's Fernet and argon2id building blocks.  The
// deployment is a wholesale Node replacement of the Python stack, so the
// formats are self-consistent rather than byte-compatible with Python:
//   * Fernet  (AES-128-CBC + HMAC-SHA256)  ->  AES-256-GCM (authenticated)
//   * argon2id                              ->  scrypt (node:crypto)
// Keys are 32 random bytes stored base64url in the key file (0400), the same
// shape as a Fernet key.  Password hashes are self-describing:
//   scrypt$N$r$p$<salt-b64url>$<hash-b64url>

import crypto from "node:crypto";

const MAGIC = Buffer.from("AESGCM1", "utf8");
const NONCE_LEN = 12;
const TAG_LEN = 16;

const SCRYPT_N = 32768;
const SCRYPT_R = 8;
const SCRYPT_P = 1;
const SCRYPT_KEYLEN = 32;
const SCRYPT_SALT_LEN = 16;

export function generateKey() {
  return crypto.randomBytes(32).toString("base64url");
}

export function encrypt(keyB64url, plaintext) {
  const key = Buffer.from(keyB64url, "base64url");
  const nonce = crypto.randomBytes(NONCE_LEN);
  const cipher = crypto.createCipheriv("aes-256-gcm", key, nonce);
  const ct = Buffer.concat([cipher.update(plaintext), cipher.final()]);
  const tag = cipher.getAuthTag();
  return Buffer.concat([MAGIC, nonce, tag, ct]);
}

export function decrypt(keyB64url, blob) {
  const key = Buffer.from(keyB64url, "base64url");
  const buf = Buffer.from(blob);
  if (
    buf.length < MAGIC.length + NONCE_LEN + TAG_LEN + 1 ||
    !buf.subarray(0, MAGIC.length).equals(MAGIC)
  ) {
    throw new Error("bad ciphertext");
  }
  const nonce = buf.subarray(MAGIC.length, MAGIC.length + NONCE_LEN);
  const tag = buf.subarray(
    MAGIC.length + NONCE_LEN,
    MAGIC.length + NONCE_LEN + TAG_LEN
  );
  const ct = buf.subarray(MAGIC.length + NONCE_LEN + TAG_LEN);
  const decipher = crypto.createDecipheriv("aes-256-gcm", key, nonce);
  decipher.setAuthTag(tag);
  return Buffer.concat([decipher.update(ct), decipher.final()]);
}

export function hashPassword(pw) {
  const salt = crypto.randomBytes(SCRYPT_SALT_LEN);
  const hash = crypto.scryptSync(pw, salt, SCRYPT_KEYLEN, {
    N: SCRYPT_N,
    r: SCRYPT_R,
    p: SCRYPT_P,
    maxmem: 128 * 1024 * 1024,
  });
  return (
    "scrypt$" +
    SCRYPT_N +
    "$" +
    SCRYPT_R +
    "$" +
    SCRYPT_P +
    "$" +
    salt.toString("base64url") +
    "$" +
    hash.toString("base64url")
  );
}

export function verifyPassword(stored, pw) {
  try {
    const parts = String(stored).split("$");
    if (parts.length !== 6 || parts[0] !== "scrypt") return false;
    const N = Number(parts[1]);
    const r = Number(parts[2]);
    const p = Number(parts[3]);
    const salt = Buffer.from(parts[4], "base64url");
    const want = Buffer.from(parts[5], "base64url");
    const got = crypto.scryptSync(String(pw), salt, want.length, {
      N,
      r,
      p,
      maxmem: 128 * 1024 * 1024,
    });
    return got.length === want.length && crypto.timingSafeEqual(got, want);
  } catch {
    return false;
  }
}

export function safeEqual(a, b) {
  const ha = crypto
    .createHash("sha256")
    .update(String(a ?? ""), "utf8")
    .digest();
  const hb = crypto
    .createHash("sha256")
    .update(String(b ?? ""), "utf8")
    .digest();
  return crypto.timingSafeEqual(ha, hb);
}
