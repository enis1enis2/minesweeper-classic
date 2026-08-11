// totp.js - RFC 6238 time-based one-time passwords + base32.
// Zero-dependency port of admin.py's totp_value / totp_verify / otpauth_uri.
// Node's Buffer has no base32 codec, so a small pure-JS codec is included.

import crypto from "node:crypto";

const B32 = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

export function base32Encode(buf) {
  let bits = 0;
  let value = 0;
  let out = "";
  for (const byte of buf) {
    value = (value << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      out += B32[(value >>> (bits - 5)) & 31];
      bits -= 5;
    }
  }
  if (bits > 0) out += B32[(value << (5 - bits)) & 31];
  return out;
}

export function base32Decode(s) {
  const clean = s.toUpperCase().replace(/\s+/g, "").replace(/=+$/, "");
  let bits = 0;
  let value = 0;
  const out = [];
  for (const ch of clean) {
    const idx = B32.indexOf(ch);
    if (idx < 0) throw new Error(`invalid base32 character: ${ch}`);
    value = (value << 5) | idx;
    bits += 5;
    if (bits >= 8) {
      out.push((value >>> (bits - 8)) & 0xff);
      bits -= 8;
    }
  }
  return Buffer.from(out);
}

export function totpValue(secretB32, counter, digits = 6) {
  const key = base32Decode(secretB32);
  const msg = Buffer.alloc(8);
  msg.writeBigUInt64BE(BigInt(counter));
  const digest = crypto.createHmac("sha1", key).update(msg).digest();
  const offset = digest[digest.length - 1] & 0x0f;
  const code = digest.readUInt32BE(offset) & 0x7fffffff;
  return String(code % 10 ** digits).padStart(digits, "0");
}

export function totpVerify(secretB32, code, { window = 1, ts } = {}) {
  if (!code || code.length !== 6 || !/^[0-9]+$/.test(code)) return false;
  if (ts === undefined) ts = Math.floor(Date.now() / 1000);
  const counter = Math.floor(ts / 30);
  for (let k = -window; k <= window; k++) {
    if (
      crypto.timingSafeEqual(
        Buffer.from(totpValue(secretB32, counter + k)),
        Buffer.from(code)
      )
    ) {
      return true;
    }
  }
  return false;
}

export function otpauthUri(username, issuer, secretB32) {
  return (
    "otpauth://totp/" +
    encodeURIComponent(username) +
    "?secret=" +
    secretB32 +
    "&issuer=" +
    encodeURIComponent(issuer)
  );
}
