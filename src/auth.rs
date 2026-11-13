//! Password hashing for the `auth_passwd` entry in `svcd.conf`.
//!
//! The stored value is salted PBKDF2-HMAC-SHA256:
//!
//! `pbkdf2-sha256$<iterations>$<salt-b64>$<hash-b64>`
//!
//! The iteration count is deliberately modest: the hash is only a local
//! administrative credential and Bearer verification runs on every request
//! from a machine client.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64;
use base64::Engine;
use sha2::{Digest, Sha256};

const PREFIX: &str = "pbkdf2-sha256";
const ITERATIONS: u32 = 10_000;
const SALT_LEN: usize = 16;

pub fn hash_password(password: &str) -> Result<String, String> {
    if password.is_empty() {
        return Err("password must not be empty".into());
    }
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).map_err(|error| format!("random salt: {}", error))?;
    let hash = derive(password.as_bytes(), &salt, ITERATIONS);
    Ok(format!(
        "{}${}${}${}",
        PREFIX,
        ITERATIONS,
        BASE64.encode(salt),
        BASE64.encode(hash)
    ))
}

pub fn verify_password(password: &str, encoded: &str) -> bool {
    let mut parts = encoded.split('$');
    if parts.next() != Some(PREFIX) {
        return false;
    }
    let iterations = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    if iterations == 0 || iterations > 1_000_000 {
        return false;
    }
    let salt = match parts.next().and_then(|value| BASE64.decode(value).ok()) {
        Some(salt) if salt.len() >= 8 => salt,
        _ => return false,
    };
    let expected = match parts.next().and_then(|value| BASE64.decode(value).ok()) {
        Some(hash) if !hash.is_empty() => hash,
        _ => return false,
    };
    if parts.next().is_some() {
        return false;
    }
    let actual = derive(password.as_bytes(), &salt, iterations);
    constant_time_eq(&actual, &expected)
}

fn derive(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block = Vec::with_capacity(salt.len() + 4);
    block.extend_from_slice(salt);
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &block);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (left, right) in out.iter_mut().zip(u.iter()) {
            *left ^= right;
        }
    }
    out
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        padded[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= padded[index];
        outer_pad[index] ^= padded[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_pbkdf2_sha256_vector() {
        let output = derive(b"password", b"salt", 1);
        let hex: String = output.iter().map(|byte| format!("{:02x}", byte)).collect();
        assert_eq!(
            hex,
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
    }

    #[test]
    fn hashes_verify_and_reject() {
        let encoded = hash_password("correct horse").expect("hash");
        assert!(encoded.starts_with("pbkdf2-sha256$"));
        assert!(verify_password("correct horse", &encoded));
        assert!(!verify_password("wrong", &encoded));
    }

    #[test]
    fn salts_are_not_reused() {
        let first = hash_password("same").expect("first");
        let second = hash_password("same").expect("second");
        assert_ne!(first, second);
        assert!(verify_password("same", &first));
        assert!(verify_password("same", &second));
    }

    #[test]
    fn malformed_or_empty_values_are_rejected() {
        for value in [
            "",
            "plain",
            "pbkdf2-sha256$0$c2FsdA$aGFzaA",
            "pbkdf2-sha256$10000$bad$bad$extra",
            "pbkdf2-sha256$10000$c2FsdA$",
        ] {
            assert!(!verify_password("x", value), "{value}");
        }
        assert!(hash_password("").is_err());
    }
}
