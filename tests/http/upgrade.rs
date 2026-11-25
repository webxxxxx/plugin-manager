//! Upgrade http tests (#264).

use super::support::target;
use super::*;

use std::path::{Path, PathBuf};

fn user_command(sandbox: &Sandbox) -> std::process::Command {
    let mut command = std::process::Command::new(&sandbox.svcd);
    sandbox.env(&mut command);
    command.env("SVCD_TEST_PRIVILEGE", "unprivileged");
    command
}

fn daemon_starts(sandbox: &Sandbox) -> usize {
    fs::read_to_string(sandbox.logs.join("svcd.log"))
        .map(|text| text.matches("svcd starting").count())
        .unwrap_or(0)
}

fn current_address(sandbox: &Sandbox) -> Option<String> {
    let text = fs::read_to_string(sandbox.data.join("run").join("http.port")).ok()?;
    let address = text.trim().to_string();
    TcpStream::connect(&address).ok().map(|_| address)
}

fn exe_suffix() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

/// Lay out an install tree the way `svcd --install` would, and point the
/// document at it. The daemon then runs from that tree, so a Windows
/// self-replace renames the running image.
fn install_tree(sandbox: &Sandbox) -> PathBuf {
    let install = sandbox.root.join("install");
    fs::create_dir_all(&install).expect("install dir");
    for name in ["svcd", "svc", "plugin", "mcp"] {
        let target = install.join(format!("{}{}", name, exe_suffix()));
        fs::copy(&sandbox.svcd, &target).expect("install binary");
    }
    let conf = sandbox.config.join("svcd.conf");
    let mut text = fs::read_to_string(&conf).unwrap_or_default();
    if !text.contains("install_dir") {
        text.push_str(&format!("install_dir = {}\n", install.display()));
    }
    fs::write(&conf, text).expect("svcd.conf");
    install
}

fn installed_in(install: &std::path::Path, name: &str) -> PathBuf {
    install.join(format!("{}{}", name, exe_suffix()))
}

/// Build a release-shaped archive that carries this test binary plus a marker,
/// so the installed file can be told apart from the original without a second
/// build (appending bytes keeps the PE/ELF runnable).
fn build_archive(sandbox: &Sandbox, marker: &str) -> PathBuf {
    let version = env!("CARGO_PKG_VERSION");
    let stage = sandbox.root.join("upgrade-stage");
    let top = format!("svcd-{}-{}", version, target());
    let root = stage.join(&top);
    fs::create_dir_all(&root).expect("stage dir");
    let mut bytes = fs::read(&sandbox.svcd).expect("read binary");
    bytes.extend_from_slice(b"\n");
    bytes.extend_from_slice(marker.as_bytes());
    for name in ["svcd", "svc", "plugin", "mcp"] {
        fs::write(root.join(format!("{}{}", name, exe_suffix())), &bytes).expect("stage binary");
    }
    let archive = sandbox.root.join("upgrade.tar.gz");
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .arg(&top)
        .status()
        .expect("run tar");
    assert!(status.success(), "tar could not build the archive");
    archive
}

#[test]
fn version_reports_the_build() {
    let sandbox = Sandbox::new();
    let mut command = std::process::Command::new(&sandbox.svcd);
    command.arg("version");
    let output = command.output().expect("run svcd version");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "version line: {text}"
    );
    assert!(text.contains("protocol 3"), "{text}");
    assert!(text.contains("svcd "), "{text}");
}

#[test]
fn a_check_without_the_store_says_so() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();

    let check = request(&address, "GET", "/api/v1/upgrade/check", "", b"");
    assert_eq!(check.status, 200, "{:?}", check.body);
    assert_eq!(check.body["store"], false, "{:?}", check.body);
    assert_eq!(check.body["current"], env!("CARGO_PKG_VERSION"));

    let mut command = user_command(&sandbox);
    command.args(["upgrade", "--check", "--json"]);
    let output = command.output().expect("run svcd upgrade --check");
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(value["store"], false, "{value}");

    let apply = request(&address, "POST", "/api/v1/upgrade/apply", "", b"");
    assert_eq!(apply.status, 400, "{:?}", apply.body);
    let message = apply.body["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("plugin-store"), "{message}");
    assert!(message.contains("--file"), "{message}");
}

#[test]
fn upgrade_routes_enforce_auth_and_methods() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_as_user();
    let address = sandbox.wait_for_http();

    assert_eq!(
        raw_request(&address, "POST", "/api/v1/upgrade/check", "", b"").status,
        405
    );
    assert_eq!(
        raw_request(&address, "GET", "/api/v1/upgrade/apply", "", b"").status,
        405
    );

    let (address, _password) = enable_auth(&mut sandbox);
    assert_eq!(
        raw_request(&address, "GET", "/api/v1/upgrade/check", "", b"").status,
        401
    );
    assert_eq!(
        raw_request(&address, "POST", "/api/v1/upgrade/apply", "", b"").status,
        401
    );
}

#[test]
fn upgrading_from_a_file_replaces_the_binaries_and_restarts() {
    let mut sandbox = Sandbox::new();
    let install = install_tree(&sandbox);
    sandbox.start_daemon_from(
        &installed_in(&install, "svcd"),
        &[("SVCD_TEST_PRIVILEGE", "unprivileged")],
    );
    sandbox.wait_for_http();
    let before = daemon_starts(&sandbox);
    assert_eq!(before, 1);
    let marker = "UPGRADE-MARKER-264";
    let archive = build_archive(&sandbox, marker);

    let mut command = user_command(&sandbox);
    command.arg("upgrade").arg("--file").arg(&archive);
    let output = support::run_bounded(&mut command, Duration::from_secs(60))
        .expect("svcd upgrade --file must finish");
    assert!(
        output.status.success(),
        "upgrade: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The installed binaries are the archive's bytes (the marker is the tail).
    for name in ["svcd", "svc", "plugin", "mcp"] {
        let path = installed_in(&install, name);
        let bytes = fs::read(&path).unwrap_or_else(|error| panic!("{}: {}", path.display(), error));
        assert!(
            bytes.ends_with(marker.as_bytes()),
            "{} was not replaced",
            path.display()
        );
    }

    // The helper stops the old daemon and starts the installed one; the new
    // process removes the `.old` leftovers at startup.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if daemon_starts(&sandbox) > before {
            if let Some(address) = current_address(&sandbox) {
                if request(&address, "GET", "/api/v1/version", "", b"").status == 200 {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        daemon_starts(&sandbox) > before,
        "the daemon did not restart"
    );
    assert!(
        !install.join(format!("svcd{}.old", exe_suffix())).exists(),
        "the .old image survived the restart"
    );
}

/// #317: a service host with no init system to lean on (`SVCD_TEST_NO_SERVICE_RESTART`
/// stands in for a `--manual` install or a Windows SCM service) installs the new
/// binaries, leaves the old daemon running, and prints the command a person has
/// to run. The upgrade itself is a success, so the exit code is 0.
#[test]
fn a_service_upgrade_without_a_restart_program_installs_and_says_so() {
    let mut sandbox = Sandbox::new();
    let install = install_tree(&sandbox);
    sandbox.start_daemon_from(
        &installed_in(&install, "svcd"),
        &[("SVCD_TEST_PRIVILEGE", "privileged")],
    );
    sandbox.wait_for_http();
    assert_eq!(daemon_starts(&sandbox), 1);
    let marker = "UPGRADE-MARKER-SERVICE";
    let archive = build_archive(&sandbox, marker);

    let mut command = std::process::Command::new(&sandbox.svcd);
    sandbox.env(&mut command);
    command.env("SVCD_TEST_PRIVILEGE", "privileged");
    command.env("SVCD_TEST_NO_SERVICE_RESTART", "1");
    command.arg("upgrade").arg("--file").arg(&archive);
    let output = support::run_bounded(&mut command, Duration::from_secs(60))
        .expect("svcd upgrade --file must finish");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("restart the svcd service yourself"), "{text}");
    let bytes = fs::read(installed_in(&install, "svcd")).expect("installed binary");
    assert!(bytes.ends_with(marker.as_bytes()));
    assert_eq!(
        daemon_starts(&sandbox),
        1,
        "a service-mode upgrade must not restart itself"
    );
}
/// A protocol-2 stand-in for `plugin-store` (#268). `release_asset` with
/// `download=false` returns the release listing; with `download=true` it copies
/// the pre-built archive or its `SHA256SUMS` into `SVCD_TEMP_DIR/incoming` and
/// answers `$blobPath`, so the host's blob materialization is exercised too.
/// Python on Unix, PowerShell on Windows, like `length_prefixed_agent`.
fn store_agent(
    listing: &str,
    archive: &Path,
    sums: &Path,
    asset: &str,
) -> (&'static str, Vec<u8>, Value) {
    if cfg!(windows) {
        let script = r#"$ErrorActionPreference = 'Stop'
$in = [Console]::OpenStandardInput()
$out = [Console]::OpenStandardOutput()
$incoming = Join-Path $env:SVCD_TEMP_DIR 'incoming'
[IO.Directory]::CreateDirectory($incoming) | Out-Null
$listing = '<LISTING>'
$len = [byte[]]::new(4)
while ($true) {
  $got = 0
  while ($got -lt 4) {
    $r = $in.Read($len, $got, 4 - $got)
    if ($r -le 0) { exit 0 }
    $got += $r
  }
  $size = ([uint32]$len[0] -shl 24) -bor ([uint32]$len[1] -shl 16) -bor ([uint32]$len[2] -shl 8) -bor [uint32]$len[3]
  $body = New-Object byte[] $size
  $got = 0
  while ($got -lt $size) {
    $r = $in.Read($body, $got, $size - $got)
    if ($r -le 0) { exit 0 }
    $got += $r
  }
  $text = [Text.Encoding]::UTF8.GetString($body)
  $id = 1
  if ($text -match '"id"\s*:\s*(\d+)') { $id = [int]$Matches[1] }
  if ($text -match '"download"\s*:\s*true') {
    if ($text -match 'SHA256SUMS') { $source = '<SUMS>'; $name = 'SHA256SUMS' }
    else { $source = '<ARCHIVE>'; $name = '<ASSET>' }
    $dest = Join-Path $incoming $name
    Copy-Item -LiteralPath $source -Destination $dest -Force
    $path = $dest.Replace('\', '\\')
    $result = '{"$blobPath":"' + $path + '"}'
  } elseif ($text -match '"method"\s*:\s*"tools/call"') {
    $payload = $listing -replace '"', '\"'
    $result = '{"content":[{"type":"text","text":"' + $payload + '"}],"isError":false}'
  } else {
    $result = '{"content":[{"type":"text","text":"ok"}],"isError":false}'
  }
  $reply = [Text.Encoding]::UTF8.GetBytes('{"jsonrpc":"2.0","id":' + $id + ',"result":' + $result + '}')
  $n = [uint32]$reply.Length
  $head = [byte[]]@((($n -shr 24) -band 255), (($n -shr 16) -band 255), (($n -shr 8) -band 255), ($n -band 255))
  $out.Write($head, 0, 4)
  $out.Write($reply, 0, $reply.Length)
  $out.Flush()
}
"#;
        let script = script
            .replace("<LISTING>", listing)
            .replace("<ARCHIVE>", &archive.display().to_string())
            .replace("<SUMS>", &sums.display().to_string())
            .replace("<ASSET>", asset);
        (
            "agent.ps1",
            script.into_bytes(),
            json!({"name": "powershell", "args": ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]}),
        )
    } else {
        let script = r#"#!/usr/bin/env python3
import json, os, shutil, struct, sys

ARCHIVE = '<ARCHIVE>'
SUMS = '<SUMS>'
LISTING = json.loads('<LISTING>')

def main():
    incoming = os.path.join(os.environ["SVCD_TEMP_DIR"], "incoming")
    os.makedirs(incoming, exist_ok=True)
    inp, out = sys.stdin.buffer, sys.stdout.buffer
    while True:
        head = inp.read(4)
        if len(head) < 4:
            break
        size = struct.unpack(">I", head)[0]
        body = inp.read(size)
        if len(body) < size:
            break
        try:
            message = json.loads(body)
        except ValueError:
            continue
        result = {"content": [{"type": "text", "text": "ok"}], "isError": False}
        if message.get("method") == "tools/call":
            arguments = (message.get("params") or {}).get("arguments") or {}
            if arguments.get("download"):
                name = arguments.get("asset")
                source = SUMS if name == "SHA256SUMS" else ARCHIVE
                destination = os.path.join(incoming, name)
                shutil.copyfile(source, destination)
                result = {"$blobPath": destination}
            else:
                # The real store answers inside an MCP content block; the host
                # must unwrap it before reading `version`.
                result = {"content": [{"type": "text", "text": json.dumps(LISTING)}], "isError": False}
        reply = json.dumps({"jsonrpc": "2.0", "id": message.get("id", 1), "result": result}).encode()
        out.write(struct.pack(">I", len(reply)))
        out.write(reply)
        out.flush()

main()
"#;
        let script = script
            .replace("<ARCHIVE>", &archive.display().to_string())
            .replace("<SUMS>", &sums.display().to_string())
            .replace("<LISTING>", listing);
        (
            "agent.py",
            script.into_bytes(),
            json!({"name": "python3", "version": ">=3.8"}),
        )
    }
}

/// Install the stand-in store through the normal HTTP install path, signed with
/// the sandbox's `test` key.
fn install_store(
    sandbox: &Sandbox,
    address: &str,
    listing: &str,
    archive: &Path,
    sums: &Path,
    asset: &str,
) {
    let key = SigningKey::from_bytes(&[0x5a; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let (name, agent, interpreter) = store_agent(listing, archive, sums, asset);
    let exec_path = format!("payload/bin/{}", name);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), agent, 0o755),
        (
            "payload/web/index.html".to_string(),
            b"<!doctype html>\n<html><body>store</body></html>".to_vec(),
            0o644,
        ),
    ];
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let manifest = json!({
        "schema": 1,
        "id": "plugin-store",
        "name": "plugin-store",
        "version": "1.0.0",
        "desc": "release feed for the upgrade tests",
        "platform": target(),
        "protocol": 2,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "interpreter": interpreter,
        "capabilities": [{
            "name": "release_asset",
            "desc": "list or download a release asset",
            "inputSchema": {"type": "object", "properties": {
                "download": {"type": "boolean"},
                "asset": {"type": "string"}
            }}
        }],
        "ui": {"type": "web", "entry": "index.html"},
    });
    let package = signed_package(&key, manifest, &files);
    let installed = request(
        address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
}

/// A running daemon in an install tree with the stand-in store installed and a
/// release archive + SHA256SUMS on disk. `with_sums` drops `sumsAsset` from the
/// listing, which is the store path's trust gate.
fn store_scene(with_sums: bool) -> (Sandbox, String, PathBuf, String) {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("");
    let install = install_tree(&sandbox);
    sandbox.start_daemon_from(
        &installed_in(&install, "svcd"),
        &[("SVCD_TEST_PRIVILEGE", "unprivileged")],
    );
    let address = sandbox.wait_for_http();
    assert_eq!(daemon_starts(&sandbox), 1);

    let archive = build_archive(&sandbox, "UPGRADE-MARKER-STORE");
    let bytes = fs::read(&archive).expect("archive");
    let asset = format!("svcd-9.9.9-{}.tar.gz", target());
    let sums = sandbox.root.join("SHA256SUMS");
    fs::write(&sums, format!("{}  {}\n", hex_sha256(&bytes), asset)).expect("sums");
    let mut listing = json!({
        "version": "9.9.9",
        "assets": [asset],
    });
    if with_sums {
        listing["sumsAsset"] = json!("SHA256SUMS");
    }
    install_store(
        &sandbox,
        &address,
        &listing.to_string(),
        &archive,
        &sums,
        &asset,
    );
    (sandbox, address, install, asset)
}

#[test]
fn upgrading_through_the_store_replaces_the_binaries_and_restarts() {
    let (sandbox, address, install, asset) = store_scene(true);

    let mut command = user_command(&sandbox);
    command.args(["upgrade", "--check", "--json"]);
    let output = support::run_bounded(&mut command, Duration::from_secs(60))
        .expect("svcd upgrade --check must finish");
    assert!(
        output.status.success(),
        "check: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let found: Value = serde_json::from_slice(&output.stdout).expect("check json");
    assert_eq!(found["store"], true, "{found}");
    assert_eq!(found["latest"], "9.9.9", "{found}");
    assert_eq!(found["asset"], asset, "{found}");
    assert_eq!(found["sumsAsset"], "SHA256SUMS", "{found}");
    assert_eq!(found["newer"], true, "{found}");
    assert_eq!(found["prerelease"], false, "{found}");

    // The console reads the same check through the HTTP face.
    let http = request(&address, "GET", "/api/v1/upgrade/check", "", b"");
    assert_eq!(http.status, 200, "{:?}", http.body);
    assert_eq!(http.body["store"], true, "{:?}", http.body);
    assert_eq!(http.body["latest"], "9.9.9", "{:?}", http.body);

    let before = daemon_starts(&sandbox);
    let mut command = user_command(&sandbox);
    command.arg("upgrade");
    let output = support::run_bounded(&mut command, Duration::from_secs(60))
        .expect("svcd upgrade must finish");
    assert!(
        output.status.success(),
        "upgrade: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    for name in ["svcd", "svc", "plugin", "mcp"] {
        let path = installed_in(&install, name);
        let bytes = fs::read(&path).unwrap_or_else(|error| panic!("{}: {}", path.display(), error));
        assert!(
            bytes.ends_with(b"UPGRADE-MARKER-STORE"),
            "{} was not replaced",
            path.display()
        );
    }

    // The helper stops the old daemon and starts the installed one; the new
    // process removes the `.old` leftovers at startup.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if daemon_starts(&sandbox) > before {
            if let Some(address) = current_address(&sandbox) {
                if request(&address, "GET", "/api/v1/version", "", b"").status == 200 {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        daemon_starts(&sandbox) > before,
        "the daemon did not restart"
    );
    assert!(
        !install.join(format!("svcd{}.old", exe_suffix())).exists(),
        "the .old image survived the restart"
    );
}

#[test]
fn a_store_release_without_checksums_is_refused() {
    let (sandbox, _address, install, _asset) = store_scene(false);

    let mut command = user_command(&sandbox);
    command.arg("upgrade");
    let output = support::run_bounded(&mut command, Duration::from_secs(60))
        .expect("svcd upgrade must finish");
    assert!(!output.status.success(), "the upgrade must be refused");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SHA256SUMS"), "{stderr}");
    assert!(stderr.contains("refusing"), "{stderr}");

    let installed = fs::read(installed_in(&install, "svcd")).expect("installed binary");
    assert!(
        !installed.ends_with(b"UPGRADE-MARKER-STORE"),
        "unsigned bytes were installed"
    );
    assert_eq!(daemon_starts(&sandbox), 1, "the daemon must not restart");
}
