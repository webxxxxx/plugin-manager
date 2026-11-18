//! `.spk` manifest, ustar parsing, signature verification and file extraction.
//!
//! A package is verified completely before anything is written to the plugin
//! tree: the manifest signature first (unless the operator explicitly opted
//! into an insecure install), then the declared `files[]` (size, sha256, mode),
//! then the tar entries. `install` can therefore keep the old version intact
//! until the new tree is complete.

#[cfg(test)]
use std::path::{Path, PathBuf};

#[cfg(test)]
use base64::engine::general_purpose::STANDARD as BASE64;
#[cfg(test)]
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::kernel::failure::{Problem, ProblemError};

pub(crate) const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_FILES: usize = 4096;

pub use crate::manage::model::manifest::*;
pub use crate::manage::model::ustar::*;
pub use crate::manage::check::verify::*;
pub use crate::manage::ops::export::*;
pub use crate::manage::ops::install::*;

pub(crate) fn check_path(p: &str) -> Result<(), ProblemError> {
    if p.starts_with('/') || p.contains('\\') || p.contains('\0') {
        return Err(Problem::Invalid.error(format!("unsafe path: {}", p)));
    }
    if p.split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(Problem::Invalid.error(format!("unsafe path: {}", p)));
    }
    Ok(())
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    hex_bytes(&Sha256::digest(bytes))
}

pub(crate) fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

/// Reproduce a package in memory for tests and the future bundler.
#[cfg(test)]
pub fn pem_from_public(key: &[u8; 32]) -> Vec<u8> {
    const PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let mut der = PREFIX.to_vec();
    der.extend_from_slice(key);
    let body = BASE64.encode(der);
    let mut pem = String::from("-----BEGIN PUBLIC KEY-----\n");
    for chunk in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).expect("base64"));
        pem.push('\n');
    }
    pem.push_str("-----END PUBLIC KEY-----\n");
    pem.into_bytes()
}

/// The one signing key every unit test that verifies a package shares. It is a
/// deterministic value, not process state: a test that needs it derives it.
#[cfg(test)]
fn test_signing_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
}

/// Install the shared test key into `trust_dir` so a package signed by
/// [`test_signed_package`] verifies without touching process state.
#[cfg(test)]
pub(crate) fn install_test_trust(trust_dir: &Path) {
    std::fs::create_dir_all(trust_dir).expect("create trust dir");
    std::fs::write(
        trust_dir.join("test.pub"),
        pem_from_public(&test_signing_key().verifying_key().to_bytes()),
    )
    .expect("write test key");
}

/// A signed package for tests outside this module: two payload files, the
/// shared test key, and the caller's name and platform.
#[cfg(test)]
pub(crate) fn test_signed_package(name: &str, platform: &str) -> Vec<u8> {
    use ed25519_dalek::Signer;

    let files: Vec<(&str, &[u8], u32)> = vec![
        ("payload/bin/demo", b"hello", 0o755),
        ("config/config", b"{}", 0o644),
    ];
    let entries: Vec<serde_json::Value> = files
        .iter()
        .map(|(path, data, mode)| {
            serde_json::json!({
                "path": path,
                "size": data.len(),
                "sha256": hex_sha256(data),
                "mode": mode,
            })
        })
        .collect();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "name": name,
        "version": "1.0.0",
        "desc": "test package",
        "platform": platform,
        "protocol": 1,
        "exec": {"path": "payload/bin/demo"},
        "files": entries,
        "config": "config/config",
    }))
    .expect("manifest");
    let signature = serde_json::to_vec(&serde_json::json!({
        "alg": "ed25519",
        "keyId": "test",
        "sig": BASE64.encode(test_signing_key().sign(&manifest).to_bytes()),
    }))
    .expect("signature");
    let tar_entries: Vec<(&str, &[u8], u32)> = vec![
        ("manifest.json", manifest.as_slice(), 0o644),
        ("signature.json", signature.as_slice(), 0o644),
        ("payload/bin/demo", files[0].1, files[0].2),
        ("config/config", files[1].1, files[1].2),
    ];
    write_tar(&tar_entries).expect("write package tar")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::{json, Value};

    /// One trust root per test binary, with the shared test key installed.
    fn trust_dir() -> &'static std::path::Path {
        static TRUST: std::sync::OnceLock<tempdir::TempDir> = std::sync::OnceLock::new();
        TRUST
            .get_or_init(|| {
                let dir = tempdir::TempDir::new();
                install_test_trust(dir.path());
                dir
            })
            .path()
    }

    // The tests call the production entry points without repeating the trust
    // root at every call site.
    fn verify_bytes(bytes: &[u8]) -> Result<Package, ProblemError> {
        super::verify_bytes(bytes, trust_dir())
    }

    fn verify_bytes_with(
        bytes: &[u8],
        check: SignatureCheck,
    ) -> Result<Package, ProblemError> {
        super::verify_bytes_with(bytes, check, trust_dir())
    }

    fn verify_bytes_with_fallback(
        bytes: &[u8],
        allow_insecure: bool,
    ) -> Result<(Package, bool), ProblemError> {
        super::verify_bytes_with_fallback(bytes, allow_insecure, trust_dir())
    }

    fn stager(staging: PathBuf, allow_insecure: bool, max_bytes: u64) -> StagingInstaller {
        StagingInstaller::new(staging, allow_insecure, max_bytes, trust_dir())
    }

    fn setup() -> SigningKey {
        test_signing_key()
    }

    type TestFiles = Vec<(&'static str, Vec<u8>, u32)>;

    fn manifest(platform: &str) -> (Vec<u8>, TestFiles) {
        let files = vec![
            ("payload/bin/demo", b"hello".to_vec(), 0o755),
            ("config/config", b"{}".to_vec(), 0o644),
        ];
        let entries: Vec<serde_json::Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({"path": path, "size": data.len(), "sha256": hex_sha256(data), "mode": mode})
            })
            .collect();
        let value = json!({
            "schema": 1,
            "name": "demo",
            "version": "1.0.0",
            "desc": "demo",
            "platform": platform,
            "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": entries,
            "config": "config/config"
        });
        let bytes = serde_json::to_vec(&value).expect("manifest");
        (bytes, files)
    }

    fn package_for(key: &SigningKey, platform: &str) -> Vec<u8> {
        let (manifest, files) = manifest(platform);
        package_from(key, "test", &manifest, &files)
    }

    fn package_from(
        key: &SigningKey,
        key_id: &str,
        manifest: &[u8],
        files: &[(&str, Vec<u8>, u32)],
    ) -> Vec<u8> {
        let sig = key.sign(manifest);
        let signature = json!({"alg":"ed25519","keyId":key_id,"sig":BASE64.encode(sig.to_bytes())});
        let signature = serde_json::to_vec(&signature).expect("signature");
        let mut tar = Vec::new();
        tar.extend_from_slice(&header("manifest.json", manifest.len(), 0o644));
        tar.extend_from_slice(manifest);
        tar.extend(std::iter::repeat_n(0, padding(manifest.len())));
        tar.extend_from_slice(&header("signature.json", signature.len(), 0o644));
        tar.extend_from_slice(&signature);
        tar.extend(std::iter::repeat_n(0, padding(signature.len())));
        for (path, data, mode) in files {
            tar.extend_from_slice(&header(path, data.len(), *mode));
            tar.extend_from_slice(data);
            tar.extend(std::iter::repeat_n(0, padding(data.len())));
        }
        tar.extend(std::iter::repeat_n(0, 1024));
        tar
    }

    fn package(key: &SigningKey) -> Vec<u8> {
        package_for(key, crate::platform::target())
    }

    fn unsigned_package_from(manifest: &[u8], files: &[(&str, Vec<u8>, u32)]) -> Vec<u8> {
        let mut entries: Vec<(&str, &[u8], u32)> = vec![("manifest.json", manifest, 0o644)];
        for (path, data, mode) in files {
            entries.push((*path, data.as_slice(), *mode));
        }
        write_tar(&entries).expect("unsigned tar")
    }

    fn header(path: &str, size: usize, mode: u32) -> [u8; 512] {
        let mut header = [0u8; 512];
        header[..path.len()].copy_from_slice(path.as_bytes());
        let mode = format!("{:07o}", mode);
        header[100..100 + mode.len()].copy_from_slice(mode.as_bytes());
        let size = format!("{:011o}", size);
        header[124..124 + size.len()].copy_from_slice(size.as_bytes());
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header
    }

    fn install_bytes(bytes: &[u8], staging: &Path) -> Result<StagedPackage, ProblemError> {
        let mut installer = stager(staging.to_path_buf(), false, 1024 * 1024);
        installer.feed(bytes)?;
        installer.finish()
    }

    #[test]
    fn streaming_install_matches_the_in_memory_path() {
        let key = setup();
        let bytes = package(&key);
        let root = tempdir::TempDir::new();
        let staged = install_bytes(&bytes, root.path()).expect("stream install");
        assert_eq!(staged.manifest.name, "demo");
        assert!(staged.signature_verified);
        assert_eq!(
            std::fs::read(root.path().join("payload/bin/demo")).expect("read"),
            b"hello"
        );

        let truncated = &bytes[..bytes.len() / 2];
        let broken = tempdir::TempDir::new();
        assert!(install_bytes(truncated, broken.path()).is_err());
    }

    #[test]
    fn a_streamed_install_accepts_any_chunk_boundary() {
        let key = setup();
        let bytes = package(&key);
        for chunk in [1usize, 3, 7, 511, 512, 513, 1000] {
            let root = tempdir::TempDir::new();
            let mut installer =
                stager(root.path().to_path_buf(), false, 1024 * 1024);
            for part in bytes.chunks(chunk) {
                installer
                    .feed(part)
                    .unwrap_or_else(|error| panic!("chunk {}: {}", chunk, error));
            }
            let staged = installer
                .finish()
                .unwrap_or_else(|error| panic!("chunk {}: {}", chunk, error));
            assert_eq!(staged.manifest.name, "demo");
            assert!(staged.signature_verified);
            assert_eq!(
                std::fs::read(root.path().join("payload/bin/demo")).expect("read"),
                b"hello"
            );
        }
    }

    #[test]
    fn a_truncated_streamed_install_removes_its_staging_directory() {
        let key = setup();
        let bytes = package(&key);
        let root = tempdir::TempDir::new();
        let staging = root.path().join("staged");
        let mut installer = stager(staging.clone(), false, 1024 * 1024);
        installer
            .feed(&bytes[..bytes.len() / 2])
            .expect("prefix feed");
        assert!(installer.finish().is_err());
        drop(installer);
        assert!(
            !staging.exists(),
            "a rejected package must not leave a staging directory"
        );
    }

    #[test]
    fn a_streamed_payload_is_hashed_against_the_manifest() {
        let key = setup();
        let mut bytes = package(&key);
        let needle = b"hello";
        let position = bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("payload");
        bytes[position] = b'j';
        let root = tempdir::TempDir::new();
        let staging = root.path().join("staged");
        let error = match install_bytes(&bytes, &staging) {
            Ok(_) => panic!("a tampered payload is rejected"),
            Err(error) => error,
        };
        assert!(error.detail().contains("sha256"), "{error}");
        assert!(!staging.exists(), "staging survives a rejected payload");
    }

    #[test]
    fn verifies_a_signed_package_and_extracts_it() {
        let key = setup();
        let package = verify_bytes(&package(&key)).expect("verify");
        assert_eq!(package.manifest.name, "demo");
        assert_eq!(package.files.len(), 2);
        let root = tempdir::TempDir::new();
        package.write_to(root.path()).expect("write");
        assert_eq!(
            std::fs::read(root.path().join("payload/bin/demo")).expect("read"),
            b"hello"
        );
    }

    #[test]
    fn unsigned_packages_need_the_insecure_switch() {
        let (manifest, files) = manifest(crate::platform::target());
        let bytes = unsigned_package_from(&manifest, &files);

        let error = verify_bytes(&bytes).expect_err("strict mode");
        assert!(error.detail().contains("signature.json"), "{error}");
        let package =
            verify_bytes_with(&bytes, SignatureCheck::AllowUnsigned).expect("insecure mode");
        assert!(package.signature_bytes.is_empty());
        let (fallback, verified) =
            verify_bytes_with_fallback(&bytes, true).expect("insecure fallback");
        assert!(!verified);
        assert!(fallback.signature_bytes.is_empty());
        assert!(verify_bytes_with_fallback(&bytes, false).is_err());

        let root = tempdir::TempDir::new();
        package.write_to(root.path()).expect("write");
        assert!(!root.path().join("signature.json").exists());
        let error = match plan_installed_spk(root.path()) {
            Ok(_) => panic!("export must be refused without a signature"),
            Err(error) => error,
        };
        assert!(error.detail().contains("no signature"), "{error}");
    }

    #[test]
    fn the_insecure_fallback_still_verifies_a_signed_package() {
        let key = setup();
        let (package, verified) =
            verify_bytes_with_fallback(&package(&key), true).expect("signed package");
        assert!(verified);
        assert!(!package.signature_bytes.is_empty());
    }

    #[test]
    fn rejects_a_tampered_manifest() {
        let key = setup();
        let mut bytes = package(&key);
        let needle = b"\"demo\"";
        let position = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("name");
        bytes[position + 1] = b'D';
        assert!(verify_bytes(&bytes).is_err());
    }

    #[test]
    fn rejects_a_tampered_file() {
        let key = setup();
        let mut bytes = package(&key);
        let needle = b"hello";
        let position = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("payload");
        bytes[position] = b'j';
        assert!(verify_bytes(&bytes).is_err());
    }

    #[test]
    fn rejects_a_platform_mismatch() {
        let key = setup();
        let mismatched = format!("{}-mismatch", crate::platform::target());
        let bytes = package_for(&key, &mismatched);
        let error = verify_bytes(&bytes).expect_err("platform");
        assert!(error.detail().contains("platform mismatch"), "{error}");
        assert_eq!(error.problem, Problem::UnsupportedPlatform);
    }

    /// The host triple can itself contain "unknown" (`x86_64-unknown-linux-gnu`).
    /// A carrier that cannot run here must stay a platform rejection (400), not
    /// become "unknown plugin" (404) — the bug the #150 handoff named.
    #[test]
    fn a_platform_mismatch_with_a_real_triple_stays_unsupported_platform() {
        let key = setup();
        let host = crate::platform::target();
        let other = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]
            .into_iter()
            .find(|candidate| *candidate != host)
            .expect("a triple that differs from this host");
        let error = verify_bytes(&package_for(&key, other)).expect_err("platform");
        assert_eq!(error.problem, Problem::UnsupportedPlatform, "{error}");
        assert!(other.contains("unknown"), "the trap only exists with a real triple");
    }

    #[test]
    fn a_broken_manifest_is_invalid_not_internal() {
        let error = parse_manifest(b"not json").expect_err("bad manifest");
        assert_eq!(error.problem, Problem::Invalid);
    }

    #[test]
    fn a_body_over_the_package_limit_is_too_large() {
        struct Sink;
        impl TarSink for Sink {
            fn entry_start(&mut self, _entry: &StreamEntry) -> Result<(), ProblemError> {
                Ok(())
            }
            fn entry_data(&mut self, _data: &[u8]) -> Result<(), ProblemError> {
                Ok(())
            }
            fn entry_end(&mut self) -> Result<(), ProblemError> {
                Ok(())
            }
        }
        let mut feed = TarFeed::new(8);
        let error = feed.feed(&[0u8; 16], &mut Sink).expect_err("over limit");
        assert_eq!(error.problem, Problem::TooLarge);
    }

    fn compat_manifest_value(
        schema: u32,
        compat: Option<serde_json::Value>,
        extra_file: Option<&str>,
    ) -> serde_json::Value {
        let mut files = vec![
            json!({"path": "payload/bin/demo", "size": 5, "sha256": hex_sha256(b"hello"), "mode": 0o755}),
            json!({"path": "config/config", "size": 2, "sha256": hex_sha256(b"{}"), "mode": 0o644}),
        ];
        if let Some(path) = extra_file {
            files.push(json!({"path": path, "size": 1, "sha256": hex_sha256(b"k"), "mode": 0o644}));
        }
        let mut value = json!({
            "schema": schema,
            "name": "demo",
            "version": "1.0.0",
            "desc": "demo",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": files,
            "config": "config/config"
        });
        if let Some(compat) = compat {
            value["compat"] = compat;
        }
        value
    }

    fn parse_and_validate(value: serde_json::Value) -> Result<Manifest, ProblemError> {
        let bytes = serde_json::to_vec(&value).expect("manifest");
        let manifest = parse_manifest(&bytes)?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }

    #[test]
    fn schema_three_accepts_a_device_list() {
        let manifest = parse_and_validate(compat_manifest_value(
            3,
            Some(json!({"devices": ["sr6110", "sr1010-v1"]})),
            None,
        ))
        .expect("compat manifest");
        assert_eq!(
            manifest.compat.as_ref().expect("compat").devices,
            ["sr6110", "sr1010-v1"]
        );
    }

    #[test]
    fn compat_requires_schema_three() {
        let error = parse_and_validate(compat_manifest_value(
            2,
            Some(json!({"devices": ["sr6110"]})),
            None,
        ))
        .expect_err("schema gate");
        assert!(error.detail().contains("compat requires schema 3"), "{error}");
    }

    #[test]
    fn a_device_list_must_be_non_empty_and_unique() {
        let empty =
            parse_and_validate(compat_manifest_value(3, Some(json!({"devices": []})), None))
                .expect_err("empty");
        assert!(empty.detail().contains("must not be empty"), "{empty}");

        let duplicate = parse_and_validate(compat_manifest_value(
            3,
            Some(json!({"devices": ["sr6110", "sr6110"]})),
            None,
        ))
        .expect_err("duplicate");
        assert!(duplicate.detail().contains("duplicate"), "{duplicate}");

        let invalid = parse_and_validate(compat_manifest_value(
            3,
            Some(json!({"devices": ["SR6110"]})),
            None,
        ))
        .expect_err("invalid");
        assert!(invalid.detail().contains("device id"), "{invalid}");
    }

    #[test]
    fn a_kernel_module_requires_a_device_list() {
        let error = parse_and_validate(compat_manifest_value(3, None, Some("payload/driver.ko")))
            .expect_err("kernel module without compat");
        assert!(error.detail().contains("kernel module"), "{error}");

        let ok = parse_and_validate(compat_manifest_value(
            3,
            Some(json!({"devices": ["sr6110"]})),
            Some("payload/driver.ko"),
        ))
        .expect("kernel module with compat");
        assert!(ok.compat.is_some());
    }

    #[test]
    fn a_kernel_module_cannot_use_an_old_schema() {
        let error = parse_and_validate(compat_manifest_value(2, None, Some("payload/driver.ko")))
            .expect_err("old schema kernel module");
        assert!(error.detail().contains("kernel module"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_key_id() {
        let key = setup();
        let (manifest, files) = manifest(crate::platform::target());
        let bytes = package_from(&key, "missing", &manifest, &files);
        let error = verify_bytes(&bytes).expect_err("unknown key");
        assert!(error.detail().contains("unknown keyId"), "{error}");
        assert!(error.detail().contains("missing.pub"), "{error}");
        assert!(error.detail().contains("trust root"), "{error}");
    }

    #[test]
    fn rejects_an_unsafe_path() {
        let key = setup();
        let data = b"payload".to_vec();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": crate::platform::target(), "protocol": 1,
            "exec": {"path": "../escape"},
            "files": [{"path": "../escape", "size": data.len(), "sha256": hex_sha256(&data), "mode": 0o755}]
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let bytes = package_from(&key, "test", &manifest, &[("../escape", data, 0o755)]);
        let error = verify_bytes(&bytes).expect_err("unsafe path");
        assert!(error.detail().contains("unsafe path"), "{error}");
    }

    #[test]
    fn rejects_a_capability_named_start() {
        let (bytes, _files) = manifest(crate::platform::target());
        let mut value: Value = serde_json::from_slice(&bytes).expect("manifest");
        value["capabilities"] = json!([
            {"name": "start", "desc": "reserved", "inputSchema": {"type": "object"}}
        ]);
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let manifest = parse_manifest(&manifest).expect("parse");

        let error = validate_manifest(&manifest).expect_err("reserved capability");

        assert!(error.detail().contains("reserved"), "{error}");
    }

    #[test]
    fn rejects_setuid_and_setgid() {
        let key = setup();
        let data = b"payload".to_vec();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": crate::platform::target(), "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": [{"path": "payload/bin/demo", "size": data.len(), "sha256": hex_sha256(&data), "mode": 0o4755}]
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let bytes = package_from(
            &key,
            "test",
            &manifest,
            &[("payload/bin/demo", data, 0o4755)],
        );
        let error = verify_bytes(&bytes).expect_err("setuid");
        assert!(error.detail().contains("setuid"), "{error}");
    }

    #[test]
    fn rejects_a_protocol_mismatch() {
        let key = setup();
        let data = b"payload".to_vec();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": crate::platform::target(), "protocol": 3,
            "exec": {"path": "payload/bin/demo"},
            "files": [{"path": "payload/bin/demo", "size": data.len(), "sha256": hex_sha256(&data), "mode": 0o755}]
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let bytes = package_from(
            &key,
            "test",
            &manifest,
            &[("payload/bin/demo", data, 0o755)],
        );
        let error = verify_bytes(&bytes).expect_err("protocol");
        assert!(error.detail().contains("protocol"), "{error}");
    }

    #[test]
    fn accepts_length_prefixed_protocol_two() {
        let key = setup();
        let data = b"payload".to_vec();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": crate::platform::target(), "protocol": 2,
            "exec": {"path": "payload/bin/demo"},
            "files": [{"path": "payload/bin/demo", "size": data.len(), "sha256": hex_sha256(&data), "mode": 0o755}]
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let bytes = package_from(
            &key,
            "test",
            &manifest,
            &[("payload/bin/demo", data, 0o755)],
        );
        verify_bytes(&bytes).expect("protocol 2 package");
    }

    fn hook_file() -> (String, Vec<u8>) {
        #[cfg(windows)]
        {
            (
                "hooks/post-install.cmd".to_string(),
                b"@echo off\r\nexit /b 0\r\n".to_vec(),
            )
        }
        #[cfg(unix)]
        {
            (
                "hooks/post-install".to_string(),
                b"#!/bin/sh\nexit 0\n".to_vec(),
            )
        }
    }

    fn hook_package(key: &SigningKey, hook_path: &str, hook_mode: u32) -> Vec<u8> {
        let hook = if cfg!(unix) {
            b"#!/bin/sh\nexit 0\n".to_vec()
        } else {
            b"@echo off\r\nexit /b 0\r\n".to_vec()
        };
        hook_package_with_body(key, hook_path, hook_mode, hook)
    }

    fn hook_package_with_body(
        key: &SigningKey,
        hook_path: &str,
        hook_mode: u32,
        hook: Vec<u8>,
    ) -> Vec<u8> {
        let exec = b"hello".to_vec();
        let files = [
            ("payload/bin/demo", exec, 0o755),
            (hook_path, hook, hook_mode),
        ];
        let entries: Vec<serde_json::Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({
                    "path": path,
                    "size": data.len(),
                    "sha256": hex_sha256(data),
                    "mode": mode
                })
            })
            .collect();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": crate::platform::target(), "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "hooks": {"postInstall": hook_path},
            "files": entries
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        package_from(key, "test", &manifest, &files)
    }

    #[test]
    fn verifies_a_package_that_declares_hooks() {
        let key = setup();
        let (path, _) = hook_file();

        let package = verify_bytes(&hook_package(&key, &path, 0o755)).expect("hooks package");

        assert_eq!(
            package
                .manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.post_install.as_deref()),
            Some(path.as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_hook_without_the_executable_bit() {
        let key = setup();
        let (path, _) = hook_file();

        let error = verify_bytes(&hook_package(&key, &path, 0o644)).expect_err("mode");

        assert!(error.detail().contains("must be executable"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_hook_without_a_shebang() {
        let key = setup();
        let (path, _) = hook_file();

        let error = verify_bytes(&hook_package_with_body(
            &key,
            &path,
            0o755,
            b"exit 0\n".to_vec(),
        ))
        .expect_err("shebang");

        assert!(error.detail().contains("shebang"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_hook_with_crlf_line_endings() {
        let key = setup();
        let (path, _) = hook_file();

        let error = verify_bytes(&hook_package_with_body(
            &key,
            &path,
            0o755,
            b"#!/bin/sh\r\nexit 0\r\n".to_vec(),
        ))
        .expect_err("crlf");

        assert!(error.detail().contains("LF line endings"), "{error}");
    }

    #[test]
    fn rejects_a_hook_outside_the_hooks_directory() {
        let key = setup();

        let error = verify_bytes(&hook_package(&key, "payload/hook", 0o755)).expect_err("path");

        assert!(error.detail().contains("must be under hooks/"), "{error}");
    }

    #[test]
    fn rejects_an_extra_tar_entry() {
        let key = setup();
        let mut bytes = package(&key);
        let end = bytes.len() - 1024;
        bytes.truncate(end);
        bytes.extend_from_slice(&header("extra.txt", 3, 0o644));
        bytes.extend_from_slice(b"bad");
        bytes.extend(std::iter::repeat_n(0, 509));
        bytes.extend(std::iter::repeat_n(0, 1024));
        let error = verify_bytes(&bytes).expect_err("extra");
        assert!(error.detail().contains("not declared"), "{error}");
    }

    #[test]
    fn rejects_a_missing_declared_file() {
        let key = setup();
        let (manifest, mut files) = manifest(crate::platform::target());
        let missing = files.pop().expect("second file");
        let bytes = package_from(&key, "test", &manifest, &files);
        let error = verify_bytes(&bytes).expect_err("missing");
        assert!(error.detail().contains("missing files"), "{error} {}", missing.0);
    }

    fn script_manifest(schema: u32) -> (Vec<u8>, TestFiles) {
        let files: TestFiles = vec![("payload/bin/clock.sh", b"#!/bin/sh\n".to_vec(), 0o755)];
        let entries: Vec<Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({"path": path, "size": data.len(), "sha256": hex_sha256(data), "mode": mode})
            })
            .collect();
        let value = json!({
            "schema": schema,
            "name": "clock",
            "version": "1.0.0",
            "desc": "clock service",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/clock.sh", "ready": "exec"},
            "interpreter": {"name": "sh"},
            "lifecycle": {
                "start": {"path": "payload/bin/clock.sh", "args": ["start"]},
                "stop": {"path": "payload/bin/clock.sh", "args": ["stop"]},
                "status": {"path": "payload/bin/clock.sh", "args": ["status"], "timeout": 5}
            },
            "files": entries
        });
        (serde_json::to_vec(&value).expect("manifest"), files)
    }

    #[test]
    fn rejects_lifecycle_or_interpreter_under_schema_one() {
        let (bytes, _) = script_manifest(1);
        let error = parse_manifest(&bytes).expect_err("schema gate");
        assert!(error.detail().contains("schema 2"), "{error}");
    }

    #[test]
    fn accepts_lifecycle_and_interpreter_under_schema_two() {
        let (bytes, _) = script_manifest(2);
        let manifest = parse_manifest(&bytes).expect("parse");
        validate_manifest(&manifest).expect("validate");
        assert_eq!(
            manifest
                .interpreter
                .as_ref()
                .map(|interpreter| interpreter.name.as_str()),
            Some("sh")
        );
        let lifecycle = manifest.lifecycle.as_ref().expect("lifecycle");
        assert_eq!(
            lifecycle.start.as_ref().expect("start").args,
            ["start".to_string()]
        );
        assert_eq!(lifecycle.status.as_ref().expect("status").timeout, Some(5));
    }

    #[test]
    fn rejects_a_lifecycle_path_outside_files_and_bad_timeouts() {
        let value = |path: &str, timeout: Option<u64>| {
            let files = vec![json!({
                "path": "payload/bin/clock.sh",
                "size": 10,
                "sha256": "00",
                "mode": 0o755
            })];
            json!({
                "schema": 2, "name": "clock", "version": "1.0.0", "desc": "clock",
                "platform": crate::platform::target(), "protocol": 1,
                "exec": {"path": "payload/bin/clock.sh", "ready": "exec"},
                "interpreter": {"name": "sh"},
                "lifecycle": {"stop": {
                    "path": path,
                    "timeout": timeout
                }},
                "files": files
            })
        };
        let bytes = serde_json::to_vec(&value("missing/stop.sh", None)).expect("json");
        let error = validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("path");
        assert!(error.detail().contains("not in files[]"), "{error}");

        let bytes = serde_json::to_vec(&value("payload/bin/clock.sh", Some(0))).expect("json");
        let error = validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("zero");
        assert!(error.detail().contains("between 1 and"), "{error}");

        let bytes = serde_json::to_vec(&value("payload/bin/clock.sh", Some(301))).expect("json");
        let error =
            validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("too long");
        assert!(error.detail().contains("between 1 and"), "{error}");
    }

    #[test]
    fn rejects_capabilities_on_an_exec_ready_plugin() {
        let (bytes, _) = script_manifest(2);
        let mut value: Value = serde_json::from_slice(&bytes).expect("json");
        value["capabilities"] = json!([{
            "name": "status", "desc": "status", "inputSchema": {}
        }]);
        let bytes = serde_json::to_vec(&value).expect("json");
        let error = validate_manifest(&parse_manifest(&bytes).expect("parse"))
            .expect_err("capabilities need a channel");
        assert!(error.detail().contains("no protocol channel"), "{error}");
    }

    #[test]
    fn rejects_a_bad_interpreter_name_or_constraint() {
        let (bytes, _) = script_manifest(2);
        let mut value: Value = serde_json::from_slice(&bytes).expect("json");
        value["interpreter"]["name"] = json!("py -3");
        let bytes = serde_json::to_vec(&value).expect("json");
        let error =
            validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("bad name");
        assert!(error.detail().contains("bare name"), "{error}");

        let mut value: Value = serde_json::from_slice(&script_manifest(2).0).expect("json");
        value["interpreter"]["version"] = json!("3.10");
        let bytes = serde_json::to_vec(&value).expect("json");
        let error =
            validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("bad constraint");
        assert!(error.detail().contains("version"), "{error}");
        assert!(error.detail().contains("must start with"), "{error}");
    }

    #[test]
    fn verify_bytes_enforces_the_schema_two_gate() {
        let key = setup();
        let (manifest, files) = script_manifest(1);
        let bytes = package_from(&key, "test", &manifest, &files);
        let error = verify_bytes(&bytes).expect_err("schema one");
        assert!(error.detail().contains("schema 2"), "{error}");
    }

    #[test]
    fn schema_two_scripts_require_an_interpreter() {
        let key = setup();
        let (manifest, files) = script_manifest(2);
        let mut value: Value = serde_json::from_slice(&manifest).expect("json");
        value.as_object_mut().expect("object").remove("interpreter");
        let manifest = serde_json::to_vec(&value).expect("json");
        let error = verify_bytes(&package_from(&key, "test", &manifest, &files))
            .expect_err("missing interpreter");
        assert!(error.detail().contains("requires an interpreter"), "{error}");
    }

    #[test]
    fn a_dangling_interpreter_on_a_native_payload_is_rejected() {
        let key = setup();
        let files: TestFiles = vec![("payload/bin/demo", b"\x7fELFdemo".to_vec(), 0o755)];
        let entries: Vec<Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({"path": path, "size": data.len(), "sha256": hex_sha256(data), "mode": mode})
            })
            .collect();
        let value = json!({
            "schema": 2,
            "name": "native",
            "version": "1.0.0",
            "desc": "native",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/demo", "ready": "notify"},
            "interpreter": {"name": "sh"},
            "files": entries
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let error = verify_bytes(&package_from(&key, "test", &manifest, &files))
            .expect_err("dangling interpreter");
        assert!(error.detail().contains("no script"), "{error}");
    }

    #[test]
    fn schema_one_scripts_keep_the_legacy_direct_exec_path() {
        let key = setup();
        let files: TestFiles = vec![("payload/bin/demo", b"#!/bin/sh\n".to_vec(), 0o755)];
        let entries: Vec<Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({"path": path, "size": data.len(), "sha256": hex_sha256(data), "mode": mode})
            })
            .collect();
        let value = json!({
            "schema": 1,
            "name": "legacy",
            "version": "1.0.0",
            "desc": "legacy",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": entries
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        verify_bytes(&package_from(&key, "test", &manifest, &files)).expect("legacy package");
    }

    #[test]
    fn verifies_a_schema_two_script_package() {
        let key = setup();
        let (manifest, files) = script_manifest(2);
        let package =
            verify_bytes(&package_from(&key, "test", &manifest, &files)).expect("script package");
        assert_eq!(
            package
                .manifest
                .lifecycle
                .as_ref()
                .and_then(|lifecycle| lifecycle.start.as_ref())
                .map(|hook| hook.path.as_str()),
            Some("payload/bin/clock.sh")
        );
    }
}

#[cfg(test)]
pub mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = crate::test_support::temp_dir().join(format!(
                "svcd-package-{}-{}-{}",
                std::process::id(),
                nanos,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
