#![cfg(test)]

use super::*;
use crate::manage::test_support::tempdir::TempDir;
use ed25519_dalek::Signature;
use std::fs;

/// A fake native payload: bundling only reads `is_native_binary`'s magic bytes,
/// so a packaging fixture never needs a host interpreter.
fn exec_payload(text: &[u8]) -> Vec<u8> {
    let mut data = vec![0x7f, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00];
    data.extend_from_slice(text);
    data
}

fn write(dir: &Path, path: &str, data: &[u8], mode: u32) {
    let path = dir.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(&path, data).expect("write file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }
}

fn pem_from_private(seed: &[u8; 32]) -> Vec<u8> {
    const PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    let mut der = PREFIX.to_vec();
    der.extend_from_slice(seed);
    let body = BASE64.encode(der);
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for chunk in body.as_bytes().chunks(48) {
        pem.push_str(std::str::from_utf8(chunk).expect("base64"));
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    pem.into_bytes()
}

fn key_file(work: &TempDir, seed: u8) -> PathBuf {
    let path = work.path().join("test.key");
    fs::write(&path, pem_from_private(&[seed; 32])).expect("key");
    path
}

fn bundle_options(dir: &Path, out: &Path, key: &Path) -> Options {
    Options {
        dir: dir.to_path_buf(),
        out: Some(out.to_path_buf()),
        key: Some(key.to_path_buf()),
        key_id: Some("test".into()),
        ..Options::default()
    }
}

fn entries(bytes: &[u8]) -> Vec<package::TarEntry> {
    package::read_tar(bytes).expect("tar")
}

fn assert_checksums(bytes: &[u8]) {
    let mut offset = 0;
    loop {
        let header = &bytes[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let stored = std::str::from_utf8(&header[148..156]).expect("checksum");
        let stored = stored.replace('\0', " ");
        let stored = usize::from_str_radix(stored.trim(), 8).expect("octal checksum");
        let mut copy = header.to_vec();
        for byte in &mut copy[148..156] {
            *byte = b' ';
        }
        let sum: usize = copy.iter().map(|byte| *byte as usize).sum();
        assert_eq!(sum, stored, "tar checksum");
        let size = std::str::from_utf8(&header[124..136]).expect("size");
        let size =
            usize::from_str_radix(size.trim_matches(char::from(0)).trim(), 8).expect("octal size");
        offset += 512 + size + (512 - size % 512) % 512;
    }
}

#[test]
fn a_kernel_module_requires_and_records_the_device_list() {
    let source = TempDir::new();
    let work = TempDir::new();
    let exec_path = if cfg!(windows) {
        "payload/bin/agent.exe"
    } else {
        "payload/bin/agent"
    };
    // Native magic keeps the script-payload rules out of the way.
    write(source.path(), exec_path, b"\x7fELF", 0o755);
    write(source.path(), "payload/driver.ko", b"\x7fELF", 0o644);
    let _key = SigningKey::from_bytes(&[31u8; 32]);
    let key_path = key_file(&work, 31);
    let out = work.path().join("out.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);

    let error = bundle(&options).expect_err("kernel module without compat");
    assert!(error.detail().contains("--compat"), "{error}");

    options.compat = vec!["sr6110".into()];
    options.platform = Some("aarch64-unknown-linux-musl".into());
    let path = bundle(&options).expect("bundle with compat");
    let bytes = fs::read(path).expect("read package");
    let entries = entries(&bytes);
    let manifest = package::parse_manifest(&entries[0].data).expect("manifest");
    assert_eq!(manifest.schema, 1);
    assert_eq!(manifest.platform, "aarch64-unknown-linux-musl");
    assert_eq!(
        manifest.compat.expect("compat").devices,
        ["sr6110".to_string()]
    );
}

#[cfg(not(unix))]
#[test]
fn cross_packaging_marks_files_under_bin_executable() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(source.path(), "payload/bin/agent", b"\x7fELF", 0o644);
    write(source.path(), "payload/bin/helper.dat", b"data", 0o644);
    write(source.path(), "config/config", b"{}", 0o644);
    let key_path = key_file(&work, 41);
    let out = work.path().join("bins.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("bins".into());
    options.exec = Some("payload/bin/agent".into());
    bundle(&options).expect("bundle");

    let bytes = fs::read(out).expect("read package");
    let manifest = package::parse_manifest(&entries(&bytes)[0].data).expect("manifest");
    let mode = |path: &str| {
        manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("missing {}", path))
            .mode
    };
    assert_eq!(mode("payload/bin/helper.dat"), 0o755);
    assert_eq!(mode("config/config"), 0o644);
}

#[test]
fn builds_a_canonical_signed_package_from_a_directory() {
    let source = TempDir::new();
    let work = TempDir::new();
    let exec_path = if cfg!(windows) {
        "payload/bin/agent.cmd"
    } else {
        "payload/bin/agent"
    };
    let script = exec_payload(b"#!/bin/sh\n");
    write(source.path(), exec_path, &script, 0o755);
    write(
        source.path(),
        "payload/web/index.html",
        b"<html></html>",
        0o644,
    );
    write(source.path(), "config/config", b"{\"a\":1}", 0o644);
    let key = SigningKey::from_bytes(&[21u8; 32]);
    let key_path = key_file(&work, 21);
    let out = work.path().join("out.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("demo".into());
    options.version = Some("1.2.3".into());
    options.desc = Some("demo plugin".into());
    options.exec = Some(exec_path.into());

    let path = bundle(&options).expect("bundle");
    assert_eq!(path, out);

    let bytes = fs::read(&out).expect("read spk");
    assert_checksums(&bytes);
    let entries = entries(&bytes);
    let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(names[0], "manifest.json");
    assert_eq!(names[1], "signature.json");
    let expected = ["config/config", exec_path, "payload/web/index.html"];
    assert_eq!(&names[2..], &expected);

    let manifest_bytes = &entries[0].data;
    assert_ne!(manifest_bytes.last(), Some(&b'\n'));
    assert!(!manifest_bytes.starts_with(&[0xef, 0xbb, 0xbf]));
    let text = std::str::from_utf8(manifest_bytes).expect("manifest utf8");
    assert!(!text.contains(": "));
    assert!(!text.contains(", "));
    let mut position = 0;
    for key in [
        "\"config\"",
        "\"desc\"",
        "\"exec\"",
        "\"files\"",
        "\"id\"",
        "\"name\"",
        "\"platform\"",
        "\"protocol\"",
        "\"schema\"",
        "\"ui\"",
        "\"version\"",
    ] {
        let found = text[position..]
            .find(key)
            .map(|offset| offset + position)
            .expect("sorted key");
        assert!(found >= position);
        position = found;
    }
    let value: Value = serde_json::from_slice(manifest_bytes).expect("manifest json");
    assert_eq!(
        serde_json::to_vec(&value).expect("canonical"),
        *manifest_bytes
    );
    let manifest = package::parse_manifest(manifest_bytes).expect("manifest");
    assert_eq!(manifest.name, "demo");
    assert_eq!(manifest.version, "1.2.3");
    assert_eq!(manifest.desc, "demo plugin");
    assert_eq!(manifest.platform, crate::platform::target());
    assert_eq!(manifest.exec.path, exec_path);
    assert_eq!(manifest.config.as_deref(), Some("config/config"));
    assert_eq!(manifest.ui.as_ref().map(|ui| ui.kind.as_str()), Some("web"));
    assert_eq!(manifest.files.len(), 3);
    let agent = manifest
        .files
        .iter()
        .find(|file| file.path == exec_path)
        .expect("exec file");
    assert_eq!(agent.mode, 0o755);
    assert_eq!(agent.size, script.len() as u64);
    assert_eq!(agent.sha256, package::hex_sha256(&script));

    let signature: Value = serde_json::from_slice(&entries[1].data).expect("signature");
    assert_eq!(signature["alg"], "ed25519");
    assert_eq!(signature["keyId"], "test");
    let signature_bytes = BASE64
        .decode(signature["sig"].as_str().expect("sig"))
        .expect("base64");
    key.verifying_key()
        .verify_strict(
            manifest_bytes,
            &Signature::from_slice(&signature_bytes).expect("signature"),
        )
        .expect("signature verifies");
}

#[test]
fn generates_a_minimal_manifest_and_detects_the_only_executable() {
    let root = TempDir::new();
    let source = root.path().join("minimal");
    fs::create_dir_all(&source).expect("source");
    let work = TempDir::new();
    let exec_name = if cfg!(windows) { "tool.cmd" } else { "tool" };
    let exec_path = format!("payload/bin/{}", exec_name);
    write(&source, &exec_path, &exec_payload(b"tool"), 0o755);
    write(&source, "README.md", b"readme", 0o644);
    write(&source, "hooks/post-install", b"#!/bin/sh\n", 0o755);
    let key_path = key_file(&work, 22);
    let out = work.path().join("out.spk");
    let mut options = bundle_options(&source, &out, &key_path);
    options.id = Some("renamed".into());

    bundle(&options).expect("bundle");
    let entries = entries(&fs::read(&out).expect("read"));
    let manifest = package::parse_manifest(&entries[0].data).expect("manifest");
    assert_eq!(manifest.id(), "renamed");
    assert_eq!(manifest.name, "renamed");
    assert_eq!(manifest.version, "0.1.0");
    assert_eq!(manifest.desc, "minimal");
    assert_eq!(manifest.exec.path, exec_path);
    assert_eq!(manifest.files.len(), 3);
    assert!(manifest.config.is_none());
    assert!(manifest.ui.is_none());
    assert_eq!(
        manifest
            .hooks
            .as_ref()
            .and_then(|hooks| hooks.post_install.as_deref()),
        Some("hooks/post-install")
    );
}

#[test]
fn requires_a_key_and_does_not_write_a_package() {
    let source = TempDir::new();
    let work = TempDir::new();
    let exec_path = if cfg!(windows) {
        "payload/bin/tool.cmd"
    } else {
        "payload/bin/tool"
    };
    write(source.path(), exec_path, &exec_payload(b"tool"), 0o755);
    let out = work.path().join("out.spk");
    let mut options = bundle_options(source.path(), &out, &work.path().join("missing.pem"));
    options.id = Some("demo".into());
    options.key = None;

    let error = bundle(&options).expect_err("missing key");
    assert!(error.detail().contains("--key"), "{error}");
    assert!(!out.exists());
}

#[test]
fn unsigned_packages_carry_no_signature_and_need_the_insecure_switch() {
    let source = TempDir::new();
    let work = TempDir::new();
    let exec_path = if cfg!(windows) {
        "payload/bin/tool.cmd"
    } else {
        "payload/bin/tool"
    };
    write(source.path(), exec_path, &exec_payload(b"tool"), 0o755);

    let out = work.path().join("unsigned.spk");
    let options = Options {
        dir: source.path().to_path_buf(),
        out: Some(out.clone()),
        id: Some("demo".into()),
        unsigned: true,
        ..Options::default()
    };
    let path = bundle(&options).expect("unsigned bundle");
    let bytes = fs::read(&path).expect("read");
    let parsed = entries(&bytes);
    let names: Vec<&str> = parsed.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(names, ["manifest.json", exec_path]);

    let inspection =
        package::inspect_bytes_with(&bytes, None, &crate::platform::test_support::temp_dir())
            .expect("inspect");
    assert_eq!(inspection.signature.state, "missing");

    // A strict install refuses it; only the host-level switch accepts it,
    // which is exactly the local-development contract of `--unsigned`.
    assert!(package::verify_bytes(&bytes, &crate::platform::test_support::temp_dir()).is_err());
    package::verify_bytes_with(
        &bytes,
        package::SignatureCheck::AllowUnsigned,
        &crate::platform::test_support::temp_dir(),
    )
    .expect("insecure acceptance");
}

#[test]
fn reports_ambiguous_or_missing_exec_candidates() {
    let work = TempDir::new();
    let key_path = key_file(&work, 23);

    let source = TempDir::new();
    let (one, two) = if cfg!(windows) {
        ("payload/bin/one.cmd", "payload/bin/two.cmd")
    } else {
        ("payload/bin/one", "payload/bin/two")
    };
    write(source.path(), one, &exec_payload(b"one"), 0o755);
    write(source.path(), two, &exec_payload(b"two"), 0o755);
    let mut ambiguous = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
    ambiguous.id = Some("ambiguous".into());
    let error = bundle(&ambiguous).expect_err("ambiguous");
    assert!(
        error.detail().contains("multiple executable candidates"),
        "{error}"
    );

    let source = TempDir::new();
    write(source.path(), "README.md", b"readme", 0o644);
    let mut missing = bundle_options(source.path(), &work.path().join("b.spk"), &key_path);
    missing.id = Some("missing".into());
    let error = bundle(&missing).expect_err("missing exec");
    assert!(
        error.detail().contains("cannot determine exec.path"),
        "{error}"
    );
}

#[test]
fn overrides_author_fields_and_recomputes_files_from_a_manifest() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/agent",
        &exec_payload(b"agent"),
        0o755,
    );
    write(
        source.path(),
        "payload/web/index.html",
        b"<html></html>",
        0o644,
    );
    write(source.path(), "config/config", b"{}", 0o644);
    let skeleton = serde_json::json!({
        "schema": 1,
        "id": "old",
        "name": "old",
        "version": "9.9.9",
        "desc": "old",
        "platform": "x86_64-unknown-linux-musl",
        "protocol": 1,
        "exec": {"path": "payload/bin/agent", "args": ["--stdio"], "ready": "notify"},
        "capabilities": [{"name": "status", "desc": "status", "inputSchema": {"type": "object"}}],
        "ui": {"type": "web", "entry": "index.html"},
        "files": [{"path": "stale", "size": 1, "sha256": "00", "mode": 420}]
    });
    fs::write(
        source.path().join("manifest.json"),
        serde_json::to_vec(&skeleton).expect("skeleton"),
    )
    .expect("manifest");
    let key_path = key_file(&work, 24);
    let out = work.path().join("out.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("fresh".into());
    options.name = Some("新插件".into());
    options.version = Some("2.0.0".into());
    options.desc = Some("fresh desc".into());

    bundle(&options).expect("bundle");
    let entries = entries(&fs::read(&out).expect("read"));
    let manifest = package::parse_manifest(&entries[0].data).expect("manifest");
    assert_eq!(manifest.id(), "fresh");
    assert_eq!(manifest.name, "新插件");
    assert_eq!(manifest.version, "2.0.0");
    assert_eq!(manifest.desc, "fresh desc");
    assert_eq!(manifest.platform, crate::platform::target());
    assert_eq!(manifest.exec.args, vec!["--stdio".to_string()]);
    assert_eq!(manifest.exec.ready.as_deref(), Some("notify"));
    assert_eq!(manifest.capabilities.len(), 1);
    assert_eq!(manifest.files.len(), 3);
    assert!(manifest.files.iter().all(|file| file.path != "stale"));
    assert!(manifest
        .files
        .iter()
        .all(|file| file.path != "manifest.json"));
    let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(
        &names[2..],
        &[
            "config/config",
            "payload/bin/agent",
            "payload/web/index.html"
        ]
    );
}

#[test]
fn rejects_an_exec_path_outside_files_and_unsafe_paths() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool",
        &exec_payload(b"tool"),
        0o755,
    );
    let key_path = key_file(&work, 26);
    let out = work.path().join("out.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("demo".into());
    options.exec = Some("payload/bin/missing".into());
    let error = bundle(&options).expect_err("missing exec");
    assert!(
        error.detail().contains("exec.path is not in files[]"),
        "{error}"
    );

    options.exec = Some("../escape".into());
    let error = bundle(&options).expect_err("unsafe exec");
    assert!(error.detail().contains("unsafe path"), "{error}");

    options.exec = Some("/absolute/tool".into());
    let error = bundle(&options).expect_err("absolute exec");
    assert!(error.detail().contains("unsafe path"), "{error}");

    options.exec = Some("payload\\bin\\tool".into());
    let error = bundle(&options).expect_err("backslash exec");
    assert!(error.detail().contains("unsafe path"), "{error}");
}

#[test]
fn rejects_a_declared_hook_that_is_not_in_files() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool",
        &exec_payload(b"tool"),
        0o755,
    );
    let skeleton = serde_json::json!({
        "schema": 1, "id": "demo", "name": "demo", "version": "1.0.0", "desc": "demo",
        "platform": "x86_64-unknown-linux-musl", "protocol": 1,
        "exec": {"path": "payload/bin/tool"},
        "hooks": {"postInstall": "hooks/post-install"}
    });
    fs::write(
        source.path().join("manifest.json"),
        serde_json::to_vec(&skeleton).expect("skeleton"),
    )
    .expect("manifest");
    let key_path = key_file(&work, 27);
    let out = work.path().join("out.spk");
    let error = bundle(&bundle_options(source.path(), &out, &key_path)).expect_err("hook");
    assert!(
        error.detail().contains("postInstall is not in files[]"),
        "{error}"
    );
}

#[test]
fn rejects_a_platform_that_is_not_a_target_triple() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool",
        &exec_payload(b"tool"),
        0o755,
    );
    let key_path = key_file(&work, 30);
    let mut options = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
    options.id = Some("demo".into());
    for platform in [
        "x86_64",
        "x86_64/unknown/linux-musl",
        "x86_64-unknown-linux/../musl",
        "x86_64 unknown linux",
    ] {
        options.platform = Some(platform.into());
        let error = bundle(&options).expect_err("invalid platform");
        assert!(error.detail().contains("invalid target triple"), "{error}");
    }
}

#[test]
fn refuses_to_write_the_package_inside_the_bundle_directory() {
    let source = TempDir::new();
    let work = TempDir::new();
    let exec_path = if cfg!(windows) {
        "payload/bin/tool.cmd"
    } else {
        "payload/bin/tool"
    };
    write(source.path(), exec_path, &exec_payload(b"tool"), 0o755);
    let key_path = key_file(&work, 32);
    let out = source.path().join("inside.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("demo".into());
    let error = bundle(&options).expect_err("output inside");
    assert!(
        error.detail().contains("outside the bundle directory"),
        "{error}"
    );
    assert!(!out.exists());

    // A directory target resolves to the same default name, so it has to hit
    // the same guard instead of writing the package into the source tree.
    let mut options = bundle_options(source.path(), source.path(), &key_path);
    options.id = Some("demo".into());
    let error = bundle(&options).expect_err("output directory inside");
    assert!(
        error.detail().contains("outside the bundle directory"),
        "{error}"
    );
    assert_eq!(fs::read_dir(source.path()).expect("read source").count(), 1);
}

#[test]
fn an_output_directory_receives_the_versioned_default_name() {
    let source = TempDir::new();
    let work = TempDir::new();
    let exec_path = if cfg!(windows) {
        "payload/bin/tool.cmd"
    } else {
        "payload/bin/tool"
    };
    write(source.path(), exec_path, &exec_payload(b"tool"), 0o755);
    let key_path = key_file(&work, 34);
    // `-o` naming an existing directory keeps the default artifact name, so an
    // output directory cannot collect unversioned files that overwrite each
    // other.
    let mut options = bundle_options(source.path(), work.path(), &key_path);
    options.id = Some("demo".into());
    let path = bundle(&options).expect("bundle into a directory");
    let bytes = fs::read(&path).expect("read package");
    let entries = entries(&bytes);
    let manifest = package::parse_manifest(&entries[0].data).expect("manifest");
    let expected = format!(
        "{}-{}-{}.spk",
        manifest.id(),
        manifest.version,
        manifest.platform
    );
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(expected.as_str())
    );
    assert!(path.is_file(), "{}", path.display());
}

#[test]
fn a_device_restricted_default_name_carries_the_first_device() {
    let source = TempDir::new();
    let work = TempDir::new();
    let exec_path = if cfg!(windows) {
        "payload/bin/tool.cmd"
    } else {
        "payload/bin/tool"
    };
    write(source.path(), exec_path, b"\x7fELF", 0o755);
    let key_path = key_file(&work, 35);
    let mut options = bundle_options(source.path(), work.path(), &key_path);
    options.id = Some("demo".into());
    options.version = Some("1.2.3".into());
    options.platform = Some("aarch64-unknown-linux-musl".into());
    options.compat = vec!["sr6110".into(), "sr6110-v2".into()];
    let path = bundle(&options).expect("bundle for a device");
    // The suffix is the first id only; the manifest keeps every id.
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("demo-1.2.3-aarch64-unknown-linux-musl-sr6110.spk")
    );
    let manifest = package::parse_manifest(&entries(&fs::read(&path).expect("read"))[0].data)
        .expect("manifest");
    assert_eq!(
        manifest.compat.expect("compat").devices,
        ["sr6110", "sr6110-v2"]
    );
}

#[test]
fn refuses_a_signing_key_inside_the_bundle_directory() {
    let source = TempDir::new();
    let exec_path = if cfg!(windows) {
        "payload/bin/tool.cmd"
    } else {
        "payload/bin/tool"
    };
    write(source.path(), exec_path, &exec_payload(b"tool"), 0o755);
    let key_path = source.path().join("signing.key");
    fs::write(&key_path, pem_from_private(&[31u8; 32])).expect("key");
    let out = source.path().join("out.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("demo".into());
    let error = bundle(&options).expect_err("key inside bundle");
    assert!(
        error.detail().contains("inside the bundle directory"),
        "{error}"
    );
    assert!(!out.exists());
}

#[cfg(unix)]
#[test]
fn rejects_a_non_executable_exec_file() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(source.path(), "payload/bin/tool", b"tool", 0o644);
    let key_path = key_file(&work, 33);
    let out = work.path().join("a.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("demo".into());
    options.exec = Some("payload/bin/tool".into());
    let error = bundle(&options).expect_err("non-executable");
    assert!(error.detail().contains("is not executable"), "{error}");
}

#[cfg(windows)]
#[test]
fn rejects_windows_hard_links() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool.cmd",
        &exec_payload(b"tool"),
        0o755,
    );
    fs::hard_link(
        source.path().join("payload/bin/tool.cmd"),
        source.path().join("payload/bin/copy.cmd"),
    )
    .expect("hard link");
    let key_path = key_file(&work, 34);
    let out = work.path().join("a.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("demo".into());
    let error = bundle(&options).expect_err("hard link");
    assert!(
        error.detail().contains("hard links are not allowed"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlinks_and_hard_links() {
    use std::os::unix::fs::symlink;

    let work = TempDir::new();
    let key_path = key_file(&work, 28);

    let source = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool",
        &exec_payload(b"tool"),
        0o755,
    );
    fs::create_dir_all(source.path().join("payload/web")).expect("web");
    symlink("payload/bin/tool", source.path().join("payload/web/alias")).expect("symlink");
    let mut options = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
    options.id = Some("symlink".into());
    let error = bundle(&options).expect_err("symlink");
    assert!(
        error.detail().contains("symbolic links are not allowed"),
        "{error}"
    );

    let source = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool",
        &exec_payload(b"tool"),
        0o755,
    );
    fs::hard_link(
        source.path().join("payload/bin/tool"),
        source.path().join("payload/bin/copy"),
    )
    .expect("hard link");
    let mut hard_link_options =
        bundle_options(source.path(), &work.path().join("b.spk"), &key_path);
    hard_link_options.id = Some("hardlink".into());
    let error = bundle(&hard_link_options).expect_err("hard link");
    assert!(
        error.detail().contains("hard links are not allowed"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn rejects_setuid_files() {
    use std::os::unix::fs::PermissionsExt;

    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool",
        &exec_payload(b"tool"),
        0o755,
    );
    fs::set_permissions(
        source.path().join("payload/bin/tool"),
        fs::Permissions::from_mode(0o4755),
    )
    .expect("setuid");
    let key_path = key_file(&work, 29);
    let mut options = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
    options.id = Some("setuid".into());
    let error = bundle(&options).expect_err("setuid");
    assert!(error.detail().contains("setuid"), "{error}");
}

#[cfg(unix)]
#[test]
fn zero_allocated_blocks_do_not_prove_sparseness() {
    // WSL1 DrvFs reports no allocated blocks for small files that do hold
    // data, so an empty block count must not reject them.
    assert!(!is_sparse(71, 0));
    assert!(!is_sparse(1 << 20, 0));
    assert!(is_sparse(1 << 20, 8));
    assert!(!is_sparse(4096, 8));
}

#[cfg(target_os = "linux")]
#[test]
fn rejects_sparse_files() {
    use std::io::{Seek, SeekFrom, Write};

    let source = TempDir::new();
    let work = TempDir::new();
    let path = source.path().join("payload/bin/tool");
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    let mut file = fs::File::create(&path).expect("create");
    file.set_len(1 << 20).expect("sparse");
    // Allocate the tail: a file with no allocated blocks at all is also
    // what WSL1 DrvFs reports for small files, so only a partly allocated
    // file proves sparseness.
    file.seek(SeekFrom::End(-8)).expect("seek");
    file.write_all(b"payload\n").expect("write");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let key_path = key_file(&work, 30);
    let mut options = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
    options.id = Some("sparse".into());
    let error = bundle(&options).expect_err("sparse");
    assert!(
        error.detail().contains("sparse files are not allowed"),
        "{error}"
    );
}

#[test]
fn rejects_a_file_over_the_package_size_limit() {
    let source = TempDir::new();
    let work = TempDir::new();
    let path = source.path().join("payload/bin/tool");
    fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    let file = fs::File::create(&path).expect("create");
    file.set_len(package::MAX_PACKAGE_BYTES + 1).expect("huge");
    drop(file);
    let key_path = key_file(&work, 31);
    let mut options = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
    options.id = Some("huge".into());
    let error = bundle(&options).expect_err("huge file");
    assert!(error.detail().contains("file exceeds"), "{error}");
}

fn script_service() -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        (
            "payload/bin/clock.cmd",
            b"@echo off\r\n:start\r\n:stop\r\n:status\r\n".to_vec(),
        )
    } else {
        (
            "payload/bin/clock.sh",
            b"#!/bin/sh\nstart() { :; }\nstop() { :; }\nstatus() { :; }\n".to_vec(),
        )
    }
}

fn script_options(source: &Path, out: &Path, key: &Path, exec: &str) -> Options {
    let mut options = bundle_options(source, out, key);
    options.id = Some("clock".into());
    options.exec = Some(exec.into());
    options.from_core = true;
    options.interpreter = Some(if cfg!(windows) {
        "cmd".into()
    } else {
        "sh".into()
    });
    if cfg!(windows) {
        options.interpreter_args = vec!["/C".into()];
    }
    options
}

#[test]
fn from_core_emits_lifecycle() {
    let source = TempDir::new();
    let work = TempDir::new();
    let (exec_path, body) = script_service();
    // The interpreter runs the script, so the source does not need +x.
    write(source.path(), exec_path, &body, 0o644);
    let key_path = key_file(&work, 40);
    let out = work.path().join("clock.spk");
    let options = script_options(source.path(), &out, &key_path, exec_path);

    bundle(&options).expect("bundle");
    let entries = entries(&fs::read(&out).expect("read"));
    let manifest = package::parse_manifest(&entries[0].data).expect("manifest");

    assert_eq!(manifest.schema, 1);
    assert_eq!(manifest.exec.ready.as_deref(), Some("exec"));
    assert_eq!(
        manifest.interpreter.as_ref().expect("interpreter").name,
        if cfg!(windows) { "cmd" } else { "sh" }
    );
    let lifecycle = manifest.lifecycle.as_ref().expect("lifecycle");
    assert_eq!(lifecycle.start.as_ref().expect("start").args, ["start"]);
    assert_eq!(lifecycle.stop.as_ref().expect("stop").args, ["stop"]);
    assert_eq!(lifecycle.status.as_ref().expect("status").args, ["status"]);
    let exec = manifest
        .files
        .iter()
        .find(|file| file.path == exec_path)
        .expect("exec file");
    assert_eq!(exec.mode, 0o755);
}

#[test]
fn detects_a_script_without_an_exec_bit_when_an_interpreter_is_declared() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/service.py",
        b"print('hi')\n",
        0o644,
    );
    let key_path = key_file(&work, 44);
    let out = work.path().join("script.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("scripty".into());
    options.from_core = true;
    options.interpreter = Some("python3".into());

    bundle(&options).expect("bundle");
    let entries = entries(&fs::read(&out).expect("read"));
    let manifest = package::parse_manifest(&entries[0].data).expect("manifest");

    assert_eq!(manifest.exec.path, "payload/bin/service.py");
    assert_eq!(manifest.schema, 1);
    assert!(manifest.lifecycle.is_none());
    assert_eq!(
        manifest
            .interpreter
            .as_ref()
            .map(|interpreter| interpreter.name.as_str()),
        Some("python3")
    );
}

#[test]
fn from_core_requires_an_interpreter_for_a_script() {
    let source = TempDir::new();
    let work = TempDir::new();
    let (exec_path, body) = script_service();
    write(source.path(), exec_path, &body, 0o644);
    let key_path = key_file(&work, 41);
    let out = work.path().join("clock.spk");
    let mut options = script_options(source.path(), &out, &key_path, exec_path);
    options.interpreter = None;
    options.interpreter_args.clear();

    let error = bundle(&options).expect_err("missing interpreter");

    assert!(error.detail().contains("--interpreter"), "{error}");
    assert!(!out.exists());
}

#[test]
fn from_core_rejects_a_native_payload() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(source.path(), "payload/bin/tool", b"\x7fELFdemo", 0o755);
    let key_path = key_file(&work, 42);
    let out = work.path().join("native.spk");
    let options = script_options(source.path(), &out, &key_path, "payload/bin/tool");

    let error = bundle(&options).expect_err("native payload");

    assert!(error.detail().contains("native binary"), "{error}");
}

#[test]
fn an_interpreter_on_a_native_payload_is_rejected() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(source.path(), "payload/bin/tool", b"\x7fELFdemo", 0o755);
    let key_path = key_file(&work, 43);
    let out = work.path().join("native.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("native".into());
    options.exec = Some("payload/bin/tool".into());
    options.interpreter = Some("sh".into());

    let error = bundle(&options).expect_err("no script payload");

    assert!(error.detail().contains("no script"), "{error}");
}

#[test]
fn the_bundler_always_writes_schema_one() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool",
        &exec_payload(b"tool"),
        0o755,
    );
    fs::write(
        source.path().join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": 3,
            "name": "演示",
            "version": "1.0.0",
            "desc": "authored as schema three",
            "platform": crate::platform::target(),
            "protocol": 1
        }))
        .expect("skeleton"),
    )
    .expect("manifest");
    let key_path = key_file(&work, 45);
    let out = work.path().join("demo.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("demo".into());
    options.exec = Some("payload/bin/tool".into());

    bundle(&options).expect("bundle");
    let bytes = fs::read(&out).expect("read");
    let manifest = package::parse_manifest(&entries(&bytes)[0].data).expect("manifest");

    assert_eq!(manifest.schema, 1);
    assert_eq!(manifest.id(), "demo");
    assert_eq!(manifest.name, "演示");
}

#[test]
fn the_display_name_defaults_to_the_id() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool",
        &exec_payload(b"tool"),
        0o755,
    );
    let key_path = key_file(&work, 46);
    let out = work.path().join("demo.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    options.id = Some("demo".into());
    options.exec = Some("payload/bin/tool".into());

    bundle(&options).expect("bundle");
    let bytes = fs::read(&out).expect("read");
    let manifest = package::parse_manifest(&entries(&bytes)[0].data).expect("manifest");

    assert_eq!(manifest.name, "demo");
}

#[test]
fn an_id_that_cannot_be_derived_is_rejected() {
    let source = TempDir::new();
    let work = TempDir::new();
    write(
        source.path(),
        "payload/bin/tool",
        &exec_payload(b"tool"),
        0o755,
    );
    let key_path = key_file(&work, 47);
    let out = work.path().join("demo.spk");
    let mut options = bundle_options(source.path(), &out, &key_path);
    // The directory name is the only id source here, and the caller supplied
    // four bytes of it through `--id`.
    options.id = Some("Not An Id".into());

    let error = bundle(&options).expect_err("bad id");
    assert!(error.detail().contains("invalid plugin id"), "{error}");
}
