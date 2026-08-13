//! Crypto helpers for the solver-auth handshake.
//!
//! HMAC-SHA256 authentication mirrors `ms/sim-server/protocol.js` exactly:
//! the server issues a 16-byte hex nonce, the client answers with
//! `HMAC_SHA256(solver_pass, "ms-auth:" + nonce)` as lowercase hex, compared
//! in constant time.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 over the given message, returned as lowercase hex.
pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key");
    mac.update(msg);
    let out = mac.finalize().into_bytes();
    out.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Constant-time string equality (length check first, then XOR-fold).
pub fn timing_safe_eq(a: &str, b: &str) -> bool {
    let ba = a.as_bytes();
    let bb = b.as_bytes();
    if ba.len() != bb.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in ba.iter().zip(bb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_known_vector() {
        // RFC 4231 test case 1: key 0x0b...0b, msg "Hi There".
        let key = [0x0bu8; 20];
        let msg = b"Hi There";
        let expected = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
        assert_eq!(hmac_sha256_hex(&key, msg), expected);
    }

    #[test]
    fn timing_safe_eq_works() {
        assert!(timing_safe_eq("abc", "abc"));
        assert!(!timing_safe_eq("abc", "abd"));
        assert!(!timing_safe_eq("abc", "abcd"));
    }
}
