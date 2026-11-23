//! Checking for and applying a release upgrade (#264).
//!
//! The daemon has no TLS and no HTTP client, so the download lives in the
//! `plugin-store` plugin; this module is the orchestration around it: ask the
//! store for the latest release, fetch the archive and `SHA256SUMS` through two
//! single-blob calls, verify the hash, unpack with the platform `tar`, swap the
//! binaries and ask for a restart.
//!
//! Nothing here checks a signature: the trust decision is "the release channel
//! produced this archive", and the hash only proves it arrived intact.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

use crate::platform::config::Config;
use crate::wire::control::channel;

/// The plugin that owns the network half of an upgrade.
pub const STORE_ID: &str = "plugin-store";
const STORE_TOOL: &str = "plugin-store.release_asset";
/// One control round trip may legitimately include a download over a slow link.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(300);

/// What an update check found. `store == false` means the plugin is missing, so
/// nothing else is meaningful.
#[derive(Debug, Clone)]
pub struct Check {
    pub store: bool,
    pub current: String,
    pub latest: Option<String>,
    pub asset: Option<String>,
    pub sums_asset: Option<String>,
    pub newer: bool,
}

impl Check {
    pub fn to_json(&self) -> Value {
        json!({
            "store": self.store,
            "current": self.current,
            "latest": self.latest,
            "asset": self.asset,
            "sumsAsset": self.sums_asset,
            "newer": self.newer,
        })
    }
}

/// What an apply produced.
#[derive(Debug, Clone)]
pub struct Applied {
    pub version: String,
    pub binary: PathBuf,
    pub restarting: bool,
    pub hint: Option<String>,
}

impl Applied {
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "applied": true,
            "version": self.version,
            "binary": self.binary.to_string_lossy(),
            "restarting": self.restarting,
        });
        if let Some(hint) = &self.hint {
            value["hint"] = json!(hint);
        }
        value
    }
}

/// `svcd <version> (<target>, protocol <n>)`.
pub fn version_line() -> String {
    format!(
        "svcd {} ({}, protocol {})",
        env!("CARGO_PKG_VERSION"),
        crate::platform::target(),
        crate::kernel::schema::MAX_PROTOCOL
    )
}

pub fn current_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Ask the store which release this host should run.
pub fn check(config: &Config) -> Result<Check, String> {
    if !store_installed(config)? {
        return Ok(Check {
            store: false,
            current: current_version(),
            latest: None,
            asset: None,
            sums_asset: None,
            newer: false,
        });
    }
    start_store(config)?;
    let listing = tool_call(config, json!({ "download": false }))?;
    let latest = listing
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "release_asset(download=false) did not return a version".to_string())?;
    let asset = pick_asset(&listing, &latest).ok_or_else(|| {
        format!(
            "release {} has no archive for {}",
            latest,
            crate::platform::target()
        )
    })?;
    let sums_asset = listing
        .get("sumsAsset")
        .and_then(Value::as_str)
        .map(str::to_string);
    let newer = compare_versions(&latest, &current_version()) > 0;
    Ok(Check {
        store: true,
        current: current_version(),
        latest: Some(latest),
        asset: Some(asset),
        sums_asset,
        newer,
    })
}

/// Download, verify, unpack, install and restart. Refuses when the release is
/// already the running one; a downgrade is allowed (the caller prints the
/// comparison).
///
/// Only the full build can do this: the download arrives as a plugin blob, and
/// blob materialization is part of the HTTP face.
#[cfg(feature = "http")]
pub fn apply_remote(config: &Config) -> Result<Applied, String> {
    let check = check(config)?;
    if !check.store {
        return Err(format!(
            "{} is not installed: install it from the plugin store, or upgrade offline with `svcd upgrade --file <archive>`",
            STORE_ID
        ));
    }
    let latest = check.latest.clone().unwrap_or_default();
    if !check.newer && latest == check.current {
        return Err(format!("already at the latest version ({})", check.current));
    }
    let asset = check.asset.clone().ok_or("no asset to download")?;
    let archive_bytes = download(config, &asset)?;
    let sums_asset = check
        .sums_asset
        .clone()
        .ok_or("the release has no SHA256SUMS asset; refusing to upgrade untrusted bytes")?;
    let sums = download(config, &sums_asset)?;
    verify_archive(&archive_bytes, &String::from_utf8_lossy(&sums), &asset)?;
    install_archive(config, &archive_bytes, &asset)?;
    let binary = installed_binary(config)?;
    let (restarting, hint) = restart(&binary)?;
    Ok(Applied {
        version: latest,
        binary,
        restarting,
        hint,
    })
}

/// The lean build has no blob store, so the in-daemon download path is closed.
#[cfg(not(feature = "http"))]
pub fn apply_remote(_config: &Config) -> Result<Applied, String> {
    Err(
        "this build has no HTTP face: upgrade offline with `svcd upgrade --file <archive>`"
            .to_string(),
    )
}

/// Offline path: a local archive, optionally with its `SHA256SUMS`.
pub fn apply_file(config: &Config, archive: &Path, sums: Option<&Path>) -> Result<Applied, String> {
    let bytes =
        fs::read(archive).map_err(|error| format!("read {}: {}", archive.display(), error))?;
    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("archive name: {}", archive.display()))?;
    if let Some(sums) = sums {
        let text = fs::read_to_string(sums)
            .map_err(|error| format!("read {}: {}", sums.display(), error))?;
        verify_archive(&bytes, &text, name)?;
    }
    let version = version_from_archive(name).unwrap_or_else(current_version);
    install_archive(config, &bytes, name)?;
    let binary = installed_binary(config)?;
    let (restarting, hint) = restart(&binary)?;
    Ok(Applied {
        version,
        binary,
        restarting,
        hint,
    })
}

/// Remove `.old` files a previous Windows self-replace left behind. Called once
/// at startup; a lock means the old process is somehow still alive, and the next
/// start tries again.
pub fn cleanup_old_files() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    for name in executable_names() {
        let _ = fs::remove_file(dir.join(format!("{}.old", name)));
    }
}

fn executable_names() -> [String; 4] {
    let suffix = std::env::consts::EXE_SUFFIX;
    [
        format!("svcd{}", suffix),
        format!("svc{}", suffix),
        format!("plugin{}", suffix),
        format!("mcp{}", suffix),
    ]
}

/// Anywhere under a cargo `target/` tree is a development build: replacing the
/// binaries there would overwrite the test/build outputs the process runs from.
fn is_development_tree(dir: &Path) -> bool {
    dir.components()
        .any(|part| part.as_os_str().to_string_lossy() == "target")
}

/// Where `svcd` lives after an install: a recorded `install_dir` wins, because
/// that is the tree `svcd --install` wrote and the one the aliases point into.
fn install_dir(config: &Config) -> Result<PathBuf, String> {
    if let Some(dir) = &config.settings.install_dir {
        return Ok(dir.clone());
    }
    let exe = std::env::current_exe().map_err(|error| format!("current executable: {}", error))?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("executable has no directory: {}", exe.display()))?;
    Ok(dir.to_path_buf())
}

fn installed_binary(config: &Config) -> Result<PathBuf, String> {
    Ok(install_dir(config)?.join(executable_names()[0].clone()))
}

/// Ask the store to look at the latest release and return the archive for this
/// target. `download=true` must name one asset; the host materializes one blob
/// per call, so the archive and `SHA256SUMS` are two calls (plugins#13).
fn tool_call(config: &Config, arguments: Value) -> Result<Value, String> {
    let response = channel::request_stream(
        config.paths.control_dir(),
        "tools/call",
        json!({ "name": STORE_TOOL, "arguments": arguments }),
        Some(CONTROL_TIMEOUT),
        || false,
        |_notification| {},
    )
    .map_err(|error| format!("svcd control request: {}", error))?;
    if let Some(error) = response.error {
        return Err(format!("{}: {}", STORE_TOOL, error.message));
    }
    let result = response
        .result
        .ok_or_else(|| format!("{} returned no result", STORE_TOOL))?;
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let message = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("the plugin reported an error");
        return Err(format!("{}: {}", STORE_TOOL, message));
    }
    Ok(result)
}

fn control(config: &Config, method: &str, params: Value) -> Result<Value, String> {
    let response = channel::request_stream(
        config.paths.control_dir(),
        method,
        params,
        Some(CONTROL_TIMEOUT),
        || false,
        |_notification| {},
    )
    .map_err(|error| format!("svcd control request: {}", error))?;
    if let Some(error) = response.error {
        return Err(format!("{}: {}", method, error.message));
    }
    Ok(response.result.unwrap_or(Value::Null))
}

fn store_installed(config: &Config) -> Result<bool, String> {
    match control(
        config,
        "svc.status",
        json!({ "id": STORE_ID, "probe": false }),
    ) {
        Ok(_) => Ok(true),
        // `svc.status` answers an unknown service as a structured problem; the
        // message is the only stable signal the control face gives here.
        Err(error) if error.contains("unknown service") => Ok(false),
        Err(error) => Err(error),
    }
}

fn start_store(config: &Config) -> Result<(), String> {
    if let Ok(status) = control(
        config,
        "svc.status",
        json!({ "id": STORE_ID, "probe": false }),
    ) {
        if status.pointer("/summary/state").and_then(Value::as_str) == Some("running") {
            return Ok(());
        }
    }
    control(config, "svc.start", json!({ "id": STORE_ID }))
        .map(|_| ())
        .map_err(|error| format!("cannot start {}: {}", STORE_ID, error))
}

#[cfg(feature = "http")]
fn download(config: &Config, asset: &str) -> Result<Vec<u8>, String> {
    let result = tool_call(config, json!({ "download": true, "asset": asset }))?;
    let id = result
        .pointer("/structuredContent/$blob")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{} returned no blob for {}", STORE_TOOL, asset))?;
    let path = crate::platform::storage::locate(&config.paths, id)
        .map_err(|error| format!("locate blob {}: {}", id, error))?;
    fs::read(&path).map_err(|error| format!("read blob {}: {}", path.display(), error))
}

fn pick_asset(listing: &Value, version: &str) -> Option<String> {
    let target = crate::platform::target();
    let wanted = [format!("-{}.zip", target), format!("-{}.tar.gz", target)];
    let names: Vec<String> = listing
        .get("assets")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|item| {
            item.as_str()
                .map(str::to_string)
                .or_else(|| item.get("name").and_then(Value::as_str).map(str::to_string))
        })
        .collect();
    for suffix in wanted {
        let candidate = names
            .iter()
            .find(|name| name.ends_with(&suffix) && name.contains(version));
        if let Some(name) = candidate {
            return Some(name.clone());
        }
    }
    names
        .iter()
        .find(|name| name.starts_with("svcd-") && name.contains(version))
        .cloned()
}

fn verify_archive(bytes: &[u8], sums: &str, asset: &str) -> Result<(), String> {
    let expected = sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        (name == asset).then(|| hash.to_ascii_lowercase())
    });
    let expected = expected.ok_or_else(|| format!("{} is not listed in SHA256SUMS", asset))?;
    let actual = crate::manage::hex_sha256(bytes);
    if actual != expected {
        return Err(format!(
            "sha256 mismatch for {}: expected {}, got {}",
            asset, expected, actual
        ));
    }
    Ok(())
}

/// Unpack with the platform `tar` (GNU tar auto-detects gzip, Windows' bsdtar
/// reads both tar.gz and zip). The archive was hash-verified first; the unpack
/// is only about laying the files out without a new decoder dependency.
fn install_archive(config: &Config, bytes: &[u8], asset: &str) -> Result<(), String> {
    let dir = install_dir(config)?;
    let dir = dir.as_path();
    if config.settings.install_dir.is_none() && is_development_tree(dir) {
        return Err(
            "refusing to upgrade a development build (started from target/): install with `svcd --install` or run the release archive"
                .to_string(),
        );
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let work = config
        .paths
        .temp_dir
        .join(format!("upgrade-{}-{}", std::process::id(), stamp));
    let unpacked_dir = work.join("unpacked");
    fs::create_dir_all(&unpacked_dir)
        .map_err(|error| format!("create {}: {}", unpacked_dir.display(), error))?;
    let archive = work.join(asset);
    fs::write(&archive, bytes)
        .map_err(|error| format!("write {}: {}", archive.display(), error))?;

    let status = Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&unpacked_dir)
        .status()
        .map_err(|error| format!("run tar: {}", error))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&work);
        return Err(format!("tar could not unpack {}", asset));
    }
    fs::create_dir_all(dir).map_err(|error| format!("create {}: {}", dir.display(), error))?;
    let root = unpack_root(&unpacked_dir)?;
    let outcome = install_files(dir, &root);
    let _ = fs::remove_dir_all(&work);
    outcome
}

fn unpack_root(dir: &Path) -> Result<PathBuf, String> {
    let binary = executable_names()[0].clone();
    if dir.join(&binary).is_file() {
        return Ok(dir.to_path_buf());
    }
    let entries =
        fs::read_dir(dir).map_err(|error| format!("read {}: {}", dir.display(), error))?;
    for entry in entries.flatten() {
        if entry.path().join(&binary).is_file() {
            return Ok(entry.path());
        }
    }
    Err(format!("{} is not in the archive", binary))
}

fn install_files(dir: &Path, unpacked: &Path) -> Result<(), String> {
    let names = executable_names();
    let binary = &names[0];
    let source = unpacked.join(binary);
    if !source.is_file() {
        return Err(format!("{} is missing from the archive", binary));
    }
    let mut staged = Vec::new();
    for name in &names {
        let from = unpacked.join(name);
        // A release always ships the aliases; an archive with only the main
        // binary still installs, with every alias a copy of it.
        let from = if from.is_file() { from } else { source.clone() };
        let to = dir.join(format!("{}.new", name));
        fs::copy(&from, &to).map_err(|error| format!("stage {}: {}", to.display(), error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&to, fs::Permissions::from_mode(0o755));
        }
        staged.push((to, dir.join(name.clone()), name.clone()));
    }
    for (staged_path, target, name) in staged {
        replace_file(&staged_path, &target, &name)?;
        if !target.is_file() {
            return Err(format!("{} did not land at {}", name, target.display()));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file(staged: &Path, target: &Path, name: &str) -> Result<(), String> {
    // A running image cannot be overwritten but can be renamed; a `.old` file
    // left from an earlier run is removed by `cleanup_old_files` on next start.
    let old = target.with_file_name(format!("{}.old", name));
    let _ = fs::remove_file(&old);
    if target.exists() {
        fs::rename(target, &old).map_err(|error| {
            format!(
                "cannot move the running {} aside: {} (a system service must stop first)",
                name, error
            )
        })?;
    }
    fs::rename(staged, target).map_err(|error| {
        let _ = fs::rename(&old, target);
        format!("cannot install {}: {}", name, error)
    })
}

#[cfg(not(windows))]
fn replace_file(staged: &Path, target: &Path, _name: &str) -> Result<(), String> {
    fs::rename(staged, target)
        .map_err(|error| format!("cannot install {}: {}", target.display(), error))
}

fn restart(binary: &Path) -> Result<(bool, Option<String>), String> {
    // The daemon restarts through the same detached helper the settings screen
    // uses, but from the *installed* path: this process may be the renamed
    // image on Windows, and its own path already points at the new file.
    if !crate::platform::paths::user_mode() {
        return Ok((
            false,
            Some(
                "upgraded; restart the service with the service manager \
                 (systemctl restart svcd / launchctl kickstart -k / sc stop svcd && sc start svcd)"
                    .to_string(),
            ),
        ));
    }
    crate::supervise::ops::restart::spawn_helper_from(binary)
        .map(|pid| (true, Some(format!("restart helper {}", pid))))
        .map_err(|error| format!("spawn restart helper: {}", error))
}

fn compare_versions(left: &str, right: &str) -> i8 {
    let parse = |value: &str| -> Vec<u64> {
        value
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let left = parse(left);
    let right = parse(right);
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        if a != b {
            return if a > b { 1 } else { -1 };
        }
    }
    0
}

/// `svcd-<version>-<target>.tar.gz` -> `<version>`.
fn version_from_archive(name: &str) -> Option<String> {
    let rest = name.strip_prefix("svcd-")?;
    let (version, _) = rest.split_once('-')?;
    Some(version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(compare_versions("0.2.0", "0.1.9"), 1);
        assert_eq!(compare_versions("0.1.10", "0.1.9"), 1);
        assert_eq!(compare_versions("0.1.5", "0.1.5"), 0);
        assert_eq!(compare_versions("0.1.5", "0.2.0"), -1);
    }

    #[test]
    fn the_version_line_names_the_build() {
        let line = version_line();
        assert!(line.starts_with("svcd "));
        assert!(line.contains(env!("CARGO_PKG_VERSION")));
        assert!(line.contains(crate::platform::target()));
    }

    #[test]
    fn a_sums_line_must_name_the_archive() {
        let sums = "aa  other.zip\nbb  svcd-0.2.0-x86_64-unknown-linux-gnu.tar.gz\n";
        assert!(verify_archive(b"x", sums, "svcd-0.2.0-x86_64-unknown-linux-gnu.tar.gz").is_err());
        let good = crate::manage::hex_sha256(b"payload");
        let sums = format!("{}  svcd-0.2.0-x.tar.gz\n", good);
        assert!(verify_archive(b"payload", &sums, "svcd-0.2.0-x.tar.gz").is_ok());
        assert!(verify_archive(b"payload", &sums, "svcd-0.3.0-x.tar.gz").is_err());
    }

    #[test]
    fn the_archive_name_carries_the_version() {
        assert_eq!(
            version_from_archive("svcd-0.2.0-x86_64-pc-windows-msvc.zip").as_deref(),
            Some("0.2.0")
        );
        assert_eq!(version_from_archive("weird.zip"), None);
    }
}
