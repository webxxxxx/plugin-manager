//! `manage` 的测试构造器：签好名的包、装好的插件树与临时目录。
//!
//! 只服务本层的单元测试（`#[cfg(test)]`），不属于生产接口。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};

use crate::kernel::failure::ProblemError;
use crate::manage::check::verify::{parse_manifest, validate_manifest};
use crate::manage::model::hex::hex_sha256;
use crate::manage::model::manifest::Manifest;
use crate::manage::model::ustar::{padding, write_tar};
use crate::manage::ops::config::write_config;
use crate::platform::paths::Paths;

/// Reproduce a package in memory for tests and the future bundler.
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

/// A fake native payload: `is_native_binary` only reads the magic bytes, so a
/// fixture that exercises install/scan never needs a host interpreter.
pub(crate) fn native_payload() -> Vec<u8> {
    let mut data = vec![0x7f, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00];
    data.extend_from_slice(b"fixture");
    data
}

/// The one signing key every unit test that verifies a package shares. It is a
/// deterministic value, not process state: a test that needs it derives it.
pub(crate) fn test_signing_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
}

/// Install the shared test key into `trust_dir` so a package signed by
/// [`test_signed_package`] verifies without touching process state.
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
pub(crate) fn test_signed_package(name: &str, platform: &str) -> Vec<u8> {
    use ed25519_dalek::Signer;

    let exec = native_payload();
    let files: Vec<(&str, &[u8], u32)> = vec![
        ("payload/bin/demo", exec.as_slice(), 0o755),
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
        "id": name,
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

/// One trust root per test binary, with the shared test key installed.
pub(crate) fn trust_dir() -> &'static std::path::Path {
    static TRUST: std::sync::OnceLock<tempdir::TempDir> = std::sync::OnceLock::new();
    TRUST
        .get_or_init(|| {
            let dir = tempdir::TempDir::new();
            install_test_trust(dir.path());
            dir
        })
        .path()
}

pub(crate) fn setup() -> SigningKey {
    test_signing_key()
}

pub(crate) type TestFiles = Vec<(&'static str, Vec<u8>, u32)>;

pub(crate) fn manifest(platform: &str) -> (Vec<u8>, TestFiles) {
    let files = vec![
        ("payload/bin/demo", native_payload(), 0o755),
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
        "id": "demo",
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

pub(crate) fn package_for(key: &SigningKey, platform: &str) -> Vec<u8> {
    let (manifest, files) = manifest(platform);
    package_from(key, "test", &manifest, &files)
}

pub(crate) fn package_from(
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

pub(crate) fn package(key: &SigningKey) -> Vec<u8> {
    package_for(key, crate::platform::target())
}

pub(crate) fn unsigned_package_from(manifest: &[u8], files: &[(&str, Vec<u8>, u32)]) -> Vec<u8> {
    let mut entries: Vec<(&str, &[u8], u32)> = vec![("manifest.json", manifest, 0o644)];
    for (path, data, mode) in files {
        entries.push((*path, data.as_slice(), *mode));
    }
    write_tar(&entries).expect("unsigned tar")
}

pub(crate) fn header(path: &str, size: usize, mode: u32) -> [u8; 512] {
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

pub(crate) fn compat_manifest_value(
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
        "schema": 1,
        "id": "demo",
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

pub(crate) fn parse_and_validate(value: serde_json::Value) -> Result<Manifest, ProblemError> {
    let bytes = serde_json::to_vec(&value).expect("manifest");
    let manifest = parse_manifest(&bytes)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub(crate) fn hook_file() -> (String, Vec<u8>) {
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

pub(crate) fn hook_package(key: &SigningKey, hook_path: &str, hook_mode: u32) -> Vec<u8> {
    let hook = if cfg!(unix) {
        b"#!/bin/sh\nexit 0\n".to_vec()
    } else {
        b"@echo off\r\nexit /b 0\r\n".to_vec()
    };
    hook_package_with_body(key, hook_path, hook_mode, hook)
}

pub(crate) fn hook_package_with_body(
    key: &SigningKey,
    hook_path: &str,
    hook_mode: u32,
    hook: Vec<u8>,
) -> Vec<u8> {
    let exec = native_payload();
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
        "schema": 1, "id": "demo", "name": "demo", "version": "1.0.0", "desc": "demo",
        "platform": crate::platform::target(), "protocol": 1,
        "exec": {"path": "payload/bin/demo"},
        "hooks": {"postInstall": hook_path},
        "files": entries
    });
    let manifest = serde_json::to_vec(&value).expect("manifest");
    package_from(key, "test", &manifest, &files)
}

pub(crate) fn script_manifest() -> (Vec<u8>, TestFiles) {
    let files: TestFiles = vec![("payload/bin/clock.sh", b"#!/bin/sh\n".to_vec(), 0o755)];
    let entries: Vec<Value> = files
        .iter()
        .map(|(path, data, mode)| {
            json!({"path": path, "size": data.len(), "sha256": hex_sha256(data), "mode": mode})
        })
        .collect();
    let value = json!({
        "schema": 1,
        "id": "clock",
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

pub(crate) mod tempdir {
    pub(crate) use crate::platform::test_support::TempDir;
}

pub(crate) static ROOT: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn root() -> &'static Path {
    let path = ROOT.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = crate::platform::test_support::temp_dir().join(format!(
            "svcd-config-unit-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(path.join("data/plugins")).expect("data");
        path
    });
    path
}

/// The directory set for the shared plugin tree: no process environment.
pub(crate) fn paths() -> &'static Paths {
    static PATHS: OnceLock<Paths> = OnceLock::new();
    PATHS.get_or_init(|| Paths::under(root()))
}

pub(crate) fn make_plugin(id: &str, config: &str, check: bool) {
    let directory = root().join("data/plugins").join(id);
    let version_dir = directory.join("1.0.0");
    std::fs::create_dir_all(version_dir.join("config")).expect("version");
    std::fs::write(directory.join("current"), "1.0.0").expect("current");
    std::fs::write(version_dir.join("config/config"), config).expect("template");
    write_config(&directory.join("config"), config.as_bytes()).expect("active");
    let exec = version_dir.join("payload/bin/demo");
    std::fs::create_dir_all(exec.parent().expect("exec dir")).expect("payload");
    std::fs::write(&exec, native_payload()).expect("exec");
    let mut manifest = serde_json::json!({
        "schema": 1,
        "id": id,
        "name": id,
        "version": "1.0.0",
        "desc": "config test",
        "platform": crate::platform::target(),
        "protocol": 1,
        "exec": {"path": "payload/bin/demo"},
        "files": []
    });
    if check {
        let path = if cfg!(windows) {
            "check/check.cmd"
        } else {
            "check/check.sh"
        };
        let script = if cfg!(windows) {
            "@echo off\r\nfindstr /C:\"good\" \"%SVCD_CONFIG%\" >nul\r\nexit /b %errorlevel%\r\n"
        } else {
            "#!/bin/sh\ngrep -q good \"$SVCD_CONFIG\"\n"
        };
        std::fs::create_dir_all(version_dir.join("check")).expect("check dir");
        std::fs::write(version_dir.join(path), script).expect("check script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                version_dir.join(path),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("chmod check");
        }
        manifest["configCheck"] = serde_json::json!({"path": path});
    }
    std::fs::write(
        version_dir.join("manifest.json"),
        serde_json::to_vec(&manifest).expect("manifest"),
    )
    .expect("write manifest");
}
