//! `.spk` 的校验与 manifest 解析：签名、trust、compat、路径与生命周期。
//!
//! 纯判定 + 读文件；不写目标树（staging 在 `manage/ops`）。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};

use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::manage::model::hex::hex_sha256;
use crate::manage::model::manifest::{
    FileData, FileEntry, Manifest, SignatureCheck, SignatureFile,
};
use crate::manage::model::manifest::{
    Inspection, Package, SignatureInfo, MAX_FILES, MAX_PACKAGE_BYTES,
};
use crate::manage::model::path::check_path;
use crate::manage::model::ustar::Tar;
use crate::wire::contract::RAW_PROTOCOL;

mod manifest;
mod signature;
mod tar;

pub(crate) use manifest::check_platform;
pub use manifest::{declares_hooks, parse_manifest, valid_name, valid_version, validate_manifest};
use signature::parse_public_key;
pub(crate) use signature::{check_signature, SignatureMode};
pub use signature::{valid_key_id, validate_public_key};
#[cfg(unix)]
pub(crate) use tar::check_hook_shebangs;
pub(crate) use tar::validate_declared_files;
use tar::verify_bytes_mode;
pub use tar::{is_native_binary, validate_script_payload};

pub fn verify_package(path: &Path, trust_dir: &Path) -> Result<Package, ProblemError> {
    verify_package_with(path, SignatureCheck::Required, trust_dir)
}

pub fn verify_package_with(
    path: &Path,
    check: SignatureCheck,
    trust_dir: &Path,
) -> Result<Package, ProblemError> {
    let bytes = fs::read(path)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    if bytes.len() as u64 > MAX_PACKAGE_BYTES {
        return Err(Problem::TooLarge.error(format!(
            "package exceeds {} MiB",
            MAX_PACKAGE_BYTES / (1024 * 1024)
        )));
    }
    verify_bytes_with(&bytes, check, trust_dir)
}

pub fn verify_bytes(bytes: &[u8], trust_dir: &Path) -> Result<Package, ProblemError> {
    verify_bytes_with(bytes, SignatureCheck::Required, trust_dir)
}

pub fn verify_bytes_with(
    bytes: &[u8],
    check: SignatureCheck,
    trust_dir: &Path,
) -> Result<Package, ProblemError> {
    let mode = match check {
        SignatureCheck::Required => SignatureMode::Required,
        SignatureCheck::AllowUnsigned => SignatureMode::AllowUnsigned,
    };
    verify_bytes_mode(bytes, mode, None, trust_dir).map(|(package, _)| package)
}

/// Verify a package's own integrity without consulting host policy: no
/// platform match, no trust decision, no interpreter. The result carries the
/// signature facts so the caller can decide whether to trust them.
///
/// `public_key` may be an explicit PEM: a CI runner has no `trust.d`, so this
/// is how it checks the release signature without pretending the key is
/// trusted. `trusted` still reports whether the key happens to be in `trust.d`.
pub fn inspect_bytes_with(
    bytes: &[u8],
    public_key: Option<&[u8]>,
    trust_dir: &Path,
) -> Result<Inspection, ProblemError> {
    let explicit = match public_key {
        Some(pem) => Some(parse_public_key(pem)?),
        None => None,
    };
    let (package, signature) =
        verify_bytes_mode(bytes, SignatureMode::Inspect, explicit.as_ref(), trust_dir)?;
    Ok(Inspection { package, signature })
}

/// Verify a package under the host-level insecure policy: strict verification
/// first, and only when it fails (and `allow_insecure` is true) retry with the
/// signature check skipped. The bool is true when the signature was verified;
/// false means the insecure fallback accepted the package.
pub fn verify_package_with_fallback(
    path: &Path,
    allow_insecure: bool,
    trust_dir: &Path,
) -> Result<(Package, bool), ProblemError> {
    if !allow_insecure {
        return verify_package(path, trust_dir).map(|package| (package, true));
    }
    match verify_package(path, trust_dir) {
        Ok(package) => Ok((package, true)),
        Err(_) => verify_package_with(path, SignatureCheck::AllowUnsigned, trust_dir)
            .map(|package| (package, false)),
    }
}

/// Byte-slice counterpart of [`verify_package_with_fallback`].
pub fn verify_bytes_with_fallback(
    bytes: &[u8],
    allow_insecure: bool,
    trust_dir: &Path,
) -> Result<(Package, bool), ProblemError> {
    if !allow_insecure {
        return verify_bytes(bytes, trust_dir).map(|package| (package, true));
    }
    match verify_bytes(bytes, trust_dir) {
        Ok(package) => Ok((package, true)),
        Err(_) => verify_bytes_with(bytes, SignatureCheck::AllowUnsigned, trust_dir)
            .map(|package| (package, false)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::ops::export::plan_installed_spk;
    use crate::manage::test_support::*;
    use serde_json::{json, Value};

    // The tests call the production entry points without repeating the trust
    // root at every call site.
    fn verify_bytes(bytes: &[u8]) -> Result<Package, ProblemError> {
        super::verify_bytes(bytes, trust_dir())
    }

    fn verify_bytes_with(bytes: &[u8], check: SignatureCheck) -> Result<Package, ProblemError> {
        super::verify_bytes_with(bytes, check, trust_dir())
    }

    fn verify_bytes_with_fallback(
        bytes: &[u8],
        allow_insecure: bool,
    ) -> Result<(Package, bool), ProblemError> {
        super::verify_bytes_with_fallback(bytes, allow_insecure, trust_dir())
    }

    #[test]
    fn declared_files_reject_more_than_the_file_ceiling() {
        let files: Vec<Value> = (0..=MAX_FILES)
            .map(|index| {
                json!({
                    "path": format!("payload/f{index}"),
                    "size": 0,
                    "sha256": hex_sha256(b""),
                    "mode": 0o644,
                })
            })
            .collect();
        let value = json!({
            "schema": 1,
            "name": "many",
            "version": "1.0.0",
            "desc": "many",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": files,
        });
        let manifest: Manifest = serde_json::from_value(value).expect("manifest");

        let error = validate_declared_files(&manifest).expect_err("over the ceiling");

        assert_eq!(error.problem, Problem::TooLarge);
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
        assert!(
            other.contains("unknown"),
            "the trap only exists with a real triple"
        );
    }

    #[test]
    fn a_broken_manifest_is_invalid_not_internal() {
        let error = parse_manifest(b"not json").expect_err("bad manifest");
        assert_eq!(error.problem, Problem::Invalid);
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
        assert!(
            error.detail().contains("compat requires schema 3"),
            "{error}"
        );
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
        assert!(
            error.detail().contains("missing files"),
            "{error} {}",
            missing.0
        );
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
        assert!(
            error.detail().contains("requires an interpreter"),
            "{error}"
        );
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
