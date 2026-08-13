use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

const B32: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn encode_base32(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() * 8 + 4) / 5);
    let mut value: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in input {
        value = (value << 8) | byte as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((value >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((value << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn decode_base32(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s
        .to_ascii_uppercase()
        .chars()
        .filter(|c| *c != ' ' && *c != '\t' && *c != '\r' && *c != '\n' && *c != '-')
        .collect();
    let mut bytes = Vec::with_capacity(clean.len() * 5 / 8);
    let mut value: u32 = 0;
    let mut bits: u32 = 0;
    for ch in clean.chars().take_while(|c| *c != '=') {
        if !ch.is_ascii() {
            return Err(format!("invalid base32 char '{ch}'"));
        }
        let idx = ch as u8;
        let v = if idx >= b'A' && idx <= b'Z' {
            idx - b'A'
        } else if idx >= b'2' && idx <= b'7' {
            idx - b'2' + 26
        } else {
            return Err(format!("invalid base32 char '{ch}'"));
        };
        value = (value << 5) | v as u32;
        bits += 5;
        if bits >= 8 {
            bytes.push((value >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Ok(bytes)
}

pub fn otpauth_uri(username: &str, issuer: &str, secret_b32: &str) -> String {
    format!(
        "otpauth://totp/{}?issuer={}&secret={}",
        encode_uri_component(username),
        encode_uri_component(issuer),
        encode_uri_component(secret_b32)
    )
}

fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*'
            | b'\'' | b'(' | b')' => out.push(b as char),
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

fn generate_secret_bytes() -> [u8; 20] {
    let mut bytes = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
}

pub fn generate_secret_b32() -> String {
    encode_base32(&generate_secret_bytes())
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    let out = mac.finalize().into_bytes();
    let mut digest = [0u8; 20];
    digest.copy_from_slice(&out);
    digest
}

fn totp_value_from_secret(secret_b32: &str, counter: u64, digits: usize) -> Option<String> {
    if digits == 0 || digits > 8 {
        return None;
    }
    let key = decode_base32(secret_b32).ok()?;
    if key.is_empty() {
        return None;
    }
    let msg = counter.to_be_bytes();
    let digest = hmac_sha1(&key, &msg);
    let offset = (digest[19] & 0x0f) as usize;
    let bin_code = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    let code = (bin_code % 10u32.pow(digits as u32)) as u64;
    Some(format!("{:0width$}", code, width = digits))
}

pub fn totp_value(secret_b32: &str, unix_time_secs: i64, digits: usize) -> Option<String> {
    let counter = unix_time_secs.div_euclid(30);
    totp_value_from_secret(secret_b32, counter as u64, digits)
}

pub fn totp_verify(secret_b32: &str, code: &str, window: usize, unix_time_secs: i64) -> bool {
    if code.is_empty() || code.len() > 8 {
        return false;
    }
    let counter = unix_time_secs.div_euclid(30) as u64;
    for offset in 0..=window {
        let c = offset as i64;
        if totp_value_from_secret(secret_b32, counter.saturating_sub(c as u64), code.len())
            .map(|v| v == code)
            .unwrap_or(false)
        {
            return true;
        }
        if offset > 0
            && totp_value_from_secret(secret_b32, counter.saturating_add(c as u64), code.len())
                .map(|v| v == code)
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_roundtrip() {
        let input: Vec<u8> = (0u8..20).collect();
        let enc = encode_base32(&input);
        assert_eq!(enc.len(), 32);
        assert_eq!(decode_base32(&enc).unwrap(), input);
    }

    #[test]
    fn base32_rejects_invalid() {
        assert!(decode_base32("A1B2C3").is_err());
    }

    #[test]
    fn base32_rfc4648_vectors() {
        assert_eq!(encode_base32(b""), "");
        assert_eq!(encode_base32(b"f"), "MY");
        assert_eq!(encode_base32(b"fo"), "MZXQ");
        assert_eq!(encode_base32(b"foo"), "MZXW6");
        assert_eq!(encode_base32(b"foob"), "MZXW6YQ");
        assert_eq!(encode_base32(b"fooba"), "MZXW6YTB");
        assert_eq!(encode_base32(b"foobar"), "MZXW6YTBOI");
    }

    #[test]
    fn totp_rfc6238_vectors() {
        // RFC 6238 Appendix B, SHA1 with seed "12345678901234567890",
        // secret = base32 of the ASCII seed.
        let seed = b"12345678901234567890";
        let secret = encode_base32(seed);
        assert_eq!(totp_value(&secret, 59, 8).unwrap(), "94287082");
        assert_eq!(totp_value(&secret, 1111111109, 8).unwrap(), "07081804");
        assert_eq!(totp_value(&secret, 1111111111, 8).unwrap(), "14050471");
        assert_eq!(totp_value(&secret, 1234567890, 8).unwrap(), "89005924");
        assert_eq!(totp_value(&secret, 2000000000, 8).unwrap(), "69279037");
        assert_eq!(totp_value(&secret, 20000000000, 8).unwrap(), "65353130");
        assert_eq!(totp_value(&secret, 59, 6).unwrap(), "287082");
    }

    #[test]
    fn totp_verify_window() {
        let seed = b"12345678901234567890";
        let secret = encode_base32(seed);
        let now = 59i64;
        assert!(totp_verify(&secret, "287082", 0, now));
        assert!(!totp_verify(&secret, "000000", 0, now));
    }

    #[test]
    fn otpauth_uri_encoding() {
        let uri = otpauth_uri("a b", "My Issuer", "SECRET");
        assert!(uri.starts_with("otpauth://totp/a%20b?issuer=My%20Issuer&secret=SECRET"));
    }
}
