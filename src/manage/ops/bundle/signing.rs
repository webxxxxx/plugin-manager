//! Signing: the Ed25519 PKCS#8 key and the signature block.
//!
//! Local development may skip this with `--unsigned`; a released package is
//! always signed with the key named by `--key`.

use std::fs;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::SigningKey;

use crate::kernel::failure::{Problem, ProblemError};

pub(super) fn default_key_id(path: &Path) -> Result<String, ProblemError> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            Problem::Invalid.error("cannot derive keyId from the key file name; pass --key-id")
        })
}

pub(super) fn read_private_key(path: &Path) -> Result<SigningKey, ProblemError> {
    let bytes = fs::read(path).map_err(|error| {
        Problem::Internal.error(format!("read key {}: {}", path.display(), error))
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        Problem::Invalid.error(format!(
            "key {} is not an Ed25519 PKCS#8 PEM",
            path.display()
        ))
    })?;
    let body = pem_body(&text, "PRIVATE KEY").ok_or_else(|| {
        Problem::Invalid.error(format!(
            "key {} is not an Ed25519 PKCS#8 PEM",
            path.display()
        ))
    })?;
    let body: String = body.lines().map(str::trim).collect();
    let der = BASE64.decode(body).map_err(|error| {
        Problem::Invalid.error(format!("key {} base64: {}", path.display(), error))
    })?;
    const PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    if der.len() != 48 || !der.starts_with(&PREFIX) {
        return Err(Problem::Invalid.error(format!(
            "key {} is not an Ed25519 PKCS#8 PEM",
            path.display()
        )));
    }
    let seed: [u8; 32] = der[16..]
        .try_into()
        .map_err(|_| Problem::Invalid.error("Ed25519 private key length"))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn pem_body<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    let begin = format!("-----BEGIN {}-----", label);
    let end = format!("-----END {}-----", label);
    let start = text.find(&begin)? + begin.len();
    let stop = text[start..].find(&end)? + start;
    Some(text[start..stop].trim())
}
