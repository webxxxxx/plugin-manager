//! Signature verification: trust-root lookup, Ed25519 checks and public-key
//! parsing.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignatureMode {
    Required,
    AllowUnsigned,
    Inspect,
}

pub(crate) fn check_signature(
    manifest: &[u8],
    signature_bytes: &[u8],
    mode: SignatureMode,
    explicit: Option<&VerifyingKey>,
    trust_dir: &Path,
) -> Result<SignatureInfo, ProblemError> {
    if mode == SignatureMode::AllowUnsigned {
        // Insecure installs skip signature verification entirely. The bytes
        // are still kept in the installed tree; they are simply not trusted.
        return Ok(SignatureInfo {
            state: "unverified",
            key_id: None,
            trusted: false,
        });
    }
    let sig: SignatureFile = serde_json::from_slice(signature_bytes)
        .map_err(|error| Problem::Invalid.error(format!("signature: {}", error)))?;
    if sig.alg != "ed25519" {
        return Err(Problem::Invalid.error(format!("unsupported signature algorithm: {}", sig.alg)));
    }
    if !valid_key_id(&sig.key_id) {
        return Err(Problem::Invalid.error(format!("invalid keyId: {}", sig.key_id)));
    }
    let key_path = trust_dir.join(format!("{}.pub", sig.key_id));
    if mode == SignatureMode::Required {
        let pem = fs::read(&key_path)
            .map_err(|error| unknown_key_error(&sig.key_id, &key_path, error))?;
        let key = parse_public_key(&pem)?;
        verify_signature_with(manifest, &sig, &key)?;
        return Ok(SignatureInfo {
            state: "valid",
            key_id: Some(sig.key_id),
            trusted: true,
        });
    }
    // Inspection: verify with whichever key is available, and never claim
    // `valid` when nothing was actually checked. On a host without the key in
    // `trust.d` (a CI runner) that is `unknown-key`; `--pubkey` lets the caller
    // supply the key explicitly instead of faking a trust root.
    let trusted_key = fs::read(&key_path)
        .ok()
        .and_then(|pem| parse_public_key(&pem).ok());
    if let Some(key) = explicit.or(trusted_key.as_ref()) {
        verify_signature_with(manifest, &sig, key)?;
        return Ok(SignatureInfo {
            state: "valid",
            key_id: Some(sig.key_id),
            trusted: trusted_key.is_some(),
        });
    }
    Ok(SignatureInfo {
        state: "unknown-key",
        key_id: Some(sig.key_id),
        trusted: false,
    })
}

fn verify_signature_with(
    manifest: &[u8],
    sig: &SignatureFile,
    key: &VerifyingKey,
) -> Result<(), ProblemError> {
    let signature = BASE64
        .decode(sig.sig.trim())
        .map_err(|error| Problem::Invalid.error(format!("signature base64: {}", error)))?;
    let signature = Signature::from_slice(&signature)
        .map_err(|error| Problem::Internal.error(error.to_string()))?;
    key.verify_strict(manifest, &signature)
        .map_err(|_| Problem::Invalid.error("manifest signature is invalid"))
}

fn unknown_key_error(key_id: &str, key_path: &Path, error: std::io::Error) -> ProblemError {
    Problem::Invalid.error(format!(
        "unknown keyId {}: {} (trust root path: {}; add it with `svc trust add <file.pub>` or upload it in the web console)",
        key_id,
        error,
        key_path.display()
    ))
}

/// Validate a PEM SPKI Ed25519 public key without installing it.
pub fn validate_public_key(pem: &[u8]) -> Result<(), ProblemError> {
    parse_public_key(pem).map(|_| ())
}

pub(super) fn parse_public_key(pem: &[u8]) -> Result<VerifyingKey, ProblemError> {
    let text = String::from_utf8_lossy(pem);
    let mut body = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("-----BEGIN PUBLIC KEY-----") {
            inside = true;
        } else if line.starts_with("-----END PUBLIC KEY-----") {
            inside = false;
        } else if inside {
            body.push_str(line.trim());
        }
    }
    let der = BASE64
        .decode(body)
        .map_err(|error| Problem::Invalid.error(format!("public key base64: {}", error)))?;
    const SPKI_PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let raw: [u8; 32] = if der.len() == 44 && der.starts_with(&SPKI_PREFIX) {
        der[12..]
            .try_into()
            .map_err(|_| Problem::Invalid.error("public key length"))?
    } else if der.len() == 32 {
        der.try_into()
            .map_err(|_| Problem::Invalid.error("public key length"))?
    } else {
        return Err(Problem::Invalid.error("public key is not an Ed25519 SPKI"));
    };
    VerifyingKey::from_bytes(&raw).map_err(|error| Problem::Internal.error(error.to_string()))
}

pub fn valid_key_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && s != "."
        && s != ".."
}
