use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use rand::RngCore;
use scrypt::{Params as ScryptParams, scrypt};
use sha2::{Digest, Sha256};

pub const MAGIC: &[u8] = b"AESGCM1";
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;
const SCRYPT_N: u32 = 32768;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
const SCRYPT_KEY_LEN: usize = 32;
const SCRYPT_SALT_LEN: usize = 16;

fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    URL_SAFE_NO_PAD
        .decode(s.trim())
        .or_else(|_| URL_SAFE.decode(s.trim()))
        .map_err(|e| format!("invalid base64: {e}"))
}

fn decode_key(key_b64url: &str) -> Result<Vec<u8>, String> {
    let key = b64url_decode(key_b64url)?;
    if key.len() != 32 {
        return Err(format!("bad key length {} (expected 32)", key.len()));
    }
    Ok(key)
}

pub fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    b64url_encode(&bytes)
}

pub fn encrypt(key_b64url: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let key = decode_key(key_b64url)?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let ct_tag = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| "AES-GCM encrypt failed".to_string())?;
    if ct_tag.len() < TAG_LEN {
        return Err("AES-GCM output too short".to_string());
    }
    let split = ct_tag.len() - TAG_LEN;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + TAG_LEN + split);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct_tag[split..]);
    out.extend_from_slice(&ct_tag[..split]);
    Ok(out)
}

pub fn decrypt(key_b64url: &str, blob: &[u8]) -> Result<Vec<u8>, String> {
    let key = decode_key(key_b64url)?;
    let min_len = MAGIC.len() + NONCE_LEN + TAG_LEN + 1;
    if blob.len() < min_len || &blob[..MAGIC.len()] != MAGIC {
        return Err("bad ciphertext (missing magic or too short)".to_string());
    }
    let nonce = &blob[MAGIC.len()..MAGIC.len() + NONCE_LEN];
    let tag_start = MAGIC.len() + NONCE_LEN;
    let tag = &blob[tag_start..tag_start + TAG_LEN];
    let ct = &blob[tag_start + TAG_LEN..];
    let mut payload = Vec::with_capacity(ct.len() + TAG_LEN);
    payload.extend_from_slice(ct);
    payload.extend_from_slice(tag);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    cipher
        .decrypt(Nonce::from_slice(nonce), payload.as_slice())
        .map_err(|_| "AES-GCM decrypt failed".to_string())
}

pub fn hash_password(password: &str) -> Result<String, String> {
    let mut salt = [0u8; SCRYPT_SALT_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    let params = ScryptParams::new(15, SCRYPT_R, SCRYPT_P, SCRYPT_KEY_LEN)
        .map_err(|e| format!("scrypt params: {e}"))?;
    let mut hash = [0u8; SCRYPT_KEY_LEN];
    scrypt(password.as_bytes(), &salt, &params, &mut hash)
        .map_err(|e| format!("scrypt: {e}"))?;
    Ok(format!(
        "scrypt${SCRYPT_N}${SCRYPT_R}${SCRYPT_P}${}${}",
        b64url_encode(&salt),
        b64url_encode(&hash)
    ))
}

pub fn verify_password(stored: &str, password: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 6 || parts[0] != "scrypt" {
        return false;
    }
    let (Ok(n), Ok(r), Ok(p)) = (
        parts[1].parse::<u32>(),
        parts[2].parse::<u32>(),
        parts[3].parse::<u32>(),
    ) else {
        return false;
    };
    if n < 2 || !n.is_power_of_two() {
        return false;
    }
    let log_n = n.trailing_zeros() as u8;
    let Ok(salt) = b64url_decode(parts[4]) else {
        return false;
    };
    let Ok(want) = b64url_decode(parts[5]) else {
        return false;
    };
    if want.is_empty() {
        return false;
    }
    let Ok(params) = ScryptParams::new(log_n, r, p, want.len()) else {
        return false;
    };
    let mut hash = vec![0u8; want.len()];
    if scrypt(password.as_bytes(), &salt, &params, &mut hash).is_err() {
        return false;
    }
    ct_eq(&hash, &want)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn safe_equal(a: &str, b: &str) -> bool {
    let mut ha = Sha256::new();
    ha.update(a.as_bytes());
    let da = ha.finalize();
    let mut hb = Sha256::new();
    hb.update(b.as_bytes());
    let db = hb.finalize();
    ct_eq(&da, &db)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_roundtrip() {
        let key = generate_key();
        let data = b"hello world this is a diagnostic blob";
        let blob = encrypt(&key, data).unwrap();
        assert!(blob.len() == MAGIC.len() + NONCE_LEN + TAG_LEN + data.len());
        let out = decrypt(&key, &blob).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn aes_tamper_detected() {
        let key = generate_key();
        let blob = encrypt(&key, b"payload").unwrap();
        let mut tampered = blob.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(decrypt(&key, &tampered).is_err());
    }

    #[test]
    fn aes_wrong_key_detected() {
        let key1 = generate_key();
        let key2 = generate_key();
        let blob = encrypt(&key1, b"payload").unwrap();
        assert!(decrypt(&key2, &blob).is_err());
    }

    #[test]
    fn aes_bad_magic() {
        let key = generate_key();
        let blob = encrypt(&key, b"payload").unwrap();
        let mut bad = blob.clone();
        bad[0] ^= 0xff;
        assert!(decrypt(&key, &bad).is_err());
    }

    #[test]
    fn scrypt_hash_and_verify() {
        let pw = "correct horse battery staple xyz";
        let hash = hash_password(pw).unwrap();
        assert!(hash.starts_with("scrypt$32768$8$1$"));
        assert!(verify_password(&hash, pw));
        assert!(!verify_password(&hash, "wrong password"));
    }

    #[test]
    fn scrypt_known_vector() {
        // Node: crypto.scryptSync("password","somesalt",64,{N:16384,r:8,p:1})
        // => 7e658cc3f7cfd7ca9355bdf81b1db930...
        let params = ScryptParams::new(14, 8, 1, 64).unwrap();
        let mut out = [0u8; 64];
        scrypt(b"password", b"somesalt", &params, &mut out).unwrap();
        let hexout: String = out.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("rust scrypt: {hexout}");
        let expect = "7e658cc3f7cfd7ca9355bdf81b1db93022254b5a7279cb230774af7f3d23559a64880d1e61bdb9ff7b7bb265ec86cecc306fc40f8bc84330385a8090f389802d";
        assert_eq!(hexout.as_str(), expect);
    }

    #[test]
    fn safe_equal_matches_and_differs() {
        assert!(safe_equal("abc", "abc"));
        assert!(!safe_equal("abc", "abd"));
        assert!(!safe_equal("abc", "abcd"));
    }

    #[test]
    fn key_must_be_32_bytes() {
        let short = b64url_encode(&[0u8; 16]);
        assert!(encrypt(&short, b"x").is_err());
    }
}
