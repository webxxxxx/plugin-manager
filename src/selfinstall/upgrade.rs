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

use super::execute::install_alias;
use crate::platform::config::Config;
#[cfg(feature = "http")]
use crate::platform::paths::Paths;
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
            "prerelease": self.latest.as_deref().map(is_prerelease).unwrap_or(false),
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
    let result = tool_call(config, json!({ "download": false }))?;
    let listing = payload(&result).ok_or_else(|| {
        format!(
            "{}: release_asset(download=false) returned no JSON listing (is the store up to date?)",
            STORE_TOOL
        )
    })?;
    let latest = listing
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{}: the listing carries no version", STORE_TOOL))?;
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
    let newer = update_available(&latest, &current_version());
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
    let archive = download(config, &asset)?;
    let sums_asset = check
        .sums_asset
        .clone()
        .ok_or("the release has no SHA256SUMS asset; refusing to upgrade untrusted bytes")?;
    let sums = download(config, &sums_asset)?;
    install_downloaded(config, &asset, archive, sums)?;
    let binary = installed_binary(config)?;
    let (restarting, hint) = restart(&binary);
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
    let (restarting, hint) = restart(&binary);
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
fn download(config: &Config, asset: &str) -> Result<Handover, String> {
    let result = tool_call(config, json!({ "download": true, "asset": asset }))?;
    let id = result
        .pointer("/structuredContent/$blob")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{} returned no blob for {}", STORE_TOOL, asset))?;
    take(&config.paths, id)
}

/// The archive (or `SHA256SUMS`) the store handed over for this attempt.
///
/// The host materializes one temp blob per `download=true` call and nothing
/// else references it, so the entry lives exactly as long as this guard.
/// Dropping it — a verified install, a hash mismatch, a `tar` that cannot
/// unpack, a target the process may not write — removes it from the temp store
/// instead of leaving it to `temp_retention_secs` (two hours by default), which
/// is how a failed upgrade pinned 15 MB on a 30 MB `/data` (#302).
#[cfg(feature = "http")]
struct Handover {
    paths: Paths,
    id: String,
    bytes: Vec<u8>,
}

#[cfg(feature = "http")]
impl Drop for Handover {
    fn drop(&mut self) {
        crate::platform::storage::release_id(&self.paths, &self.id);
    }
}

/// Read one handover and arm its release.
///
/// The guard exists before the read, so a blob that cannot be read is still
/// removed on the way out. A missing blob is not a leak, and a failed attempt
/// stays retryable: the store downloads the archive again.
#[cfg(feature = "http")]
fn take(paths: &Paths, id: &str) -> Result<Handover, String> {
    let mut handover = Handover {
        paths: paths.clone(),
        id: id.to_string(),
        bytes: Vec::new(),
    };
    let path = crate::platform::storage::locate(paths, id)
        .map_err(|error| format!("locate blob {}: {}", id, error))?;
    handover.bytes =
        fs::read(&path).map_err(|error| format!("read blob {}: {}", path.display(), error))?;
    Ok(handover)
}

/// Verify and install one handed-over archive.
///
/// Both guards are owned here, so every exit path releases the blobs: the
/// archive once it is unpacked, both after any error.
#[cfg(feature = "http")]
fn install_downloaded(
    config: &Config,
    asset: &str,
    archive: Handover,
    sums: Handover,
) -> Result<(), String> {
    verify_archive(&archive.bytes, &String::from_utf8_lossy(&sums.bytes), asset)?;
    install_archive(config, &archive.bytes, asset)
}

/// The capability's JSON payload.
///
/// `plugin-store` answers through the MCP content block: its `release_asset`
/// result is `{"content":[{"type":"text","text":"<json>"}],"isError":false}`,
/// so the listing lives in `content[0].text`. A protocol-2 stand-in (and any
/// future plugin that predates the wrapper) hands the payload back as the raw
/// result instead; both shapes are accepted. A blob result never reaches this
/// point: the host rewrites it into `structuredContent` before the call
/// returns.
fn payload(result: &Value) -> Option<Value> {
    if let Some(text) = result.pointer("/content/0/text").and_then(Value::as_str) {
        return serde_json::from_str(text).ok();
    }
    if result.get("content").is_some() {
        return None;
    }
    Some(result.clone())
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

    let output = Command::new("tar")
        .arg(tar_mode(bytes))
        .arg(&archive)
        .arg("-C")
        .arg(&unpacked_dir)
        .output()
        .map_err(|error| format!("run tar: {}", error))?;
    if !output.status.success() {
        let _ = fs::remove_dir_all(&work);
        // The console shows the reason inline: a device user has no shell to
        // read `tar`'s stderr from, and "invalid tar magic" is not something
        // they can act on without seeing it (#302).
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            return Err(format!("tar could not unpack {}", asset));
        }
        return Err(format!("tar could not unpack {}: {}", asset, detail));
    }
    fs::create_dir_all(dir).map_err(|error| format!("create {}: {}", dir.display(), error))?;
    let root = unpack_root(&unpacked_dir)?;
    let outcome = install_files(dir, &root);
    let _ = fs::remove_dir_all(&work);
    outcome
}

/// The flags that unpack these bytes.
///
/// BusyBox `tar` — the OpenWrt default — has no auto-detection: plain `-x` reads
/// a gzip stream as a tar header and dies with `invalid tar magic`, so a device
/// upgrade failed even though `tar` was installed and the archive was intact.
/// GNU tar and Windows' bsdtar both accept the explicit `-z`; the check is on
/// the content rather than the file name because `svcd upgrade --file` accepts
/// any local archive.
fn tar_mode(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        "-xzf"
    } else {
        "-xf"
    }
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
    let target = dir.join(binary);
    // Stage the main binary once and let the aliases share it: staging all four
    // names cost four binaries — 5.7 MB of a 30 MB `/data` (#304).
    let staged = dir.join(format!("{}.new", binary));
    stage_file(&source, &staged)?;
    replace_file(&staged, &target, binary)?;
    if !target.is_file() {
        return Err(format!("{} did not land at {}", binary, target.display()));
    }
    for name in &names[1..] {
        let path = dir.join(name);
        let from = unpacked.join(name);
        // A release ships the aliases as hard links to `svcd`; an archive that
        // expanded them carries independent files, installed as they are. An
        // archive with no alias still installs, every alias a link to `svcd`.
        if from.is_file() && !shares_inode_with(&from, &source) {
            let staged = dir.join(format!("{}.new", name));
            stage_file(&from, &staged)?;
            replace_file(&staged, &path, name)?;
        } else {
            install_alias(&target, &path)
                .map_err(|error| format!("cannot install {}: {}", path.display(), error))?;
        }
        if !path.is_file() {
            return Err(format!("{} did not land at {}", name, path.display()));
        }
    }
    Ok(())
}

/// Copy `from` to a sibling that [`replace_file`] can rename into place.
fn stage_file(from: &Path, staged: &Path) -> Result<(), String> {
    fs::copy(from, staged).map_err(|error| format!("stage {}: {}", staged.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(staged, fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Whether `tar` restored `alias` as a hard link to `main`, which is how the
/// release archive ships the three names.
#[cfg(unix)]
fn shares_inode_with(alias: &Path, main: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(alias), fs::metadata(main)) {
        (Ok(alias), Ok(main)) => alias.dev() == main.dev() && alias.ino() == main.ino(),
        _ => false,
    }
}

/// Windows keeps the aliases as separate files and `install_alias` copies
/// there, so what the archive shipped is what gets installed.
#[cfg(not(unix))]
fn shares_inode_with(_alias: &Path, _main: &Path) -> bool {
    false
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

/// Ask for the restart after the binaries were swapped.
///
/// The installed path is what the helper must run: this process may be the
/// renamed image on Windows, and its own path already points at the new file.
/// The route is the one every restart site takes (#317), so a host that cannot
/// restart itself gets the manual command instead of a failed upgrade.
fn restart(binary: &Path) -> (bool, Option<String>) {
    let restarted = crate::selfinstall::restart_now(Some(binary));
    (restarted.helper.is_some(), restarted.hint)
}

/// A tag with a `-` suffix (`0.2.0-rc1`) is a prerelease: the console never
/// offers it, and `svcd upgrade` does not pick it up on its own.
pub fn is_prerelease(version: &str) -> bool {
    version.contains('-')
}

fn update_available(latest: &str, current: &str) -> bool {
    !is_prerelease(latest) && compare_versions(latest, current) > 0
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

    /// The store answers through the MCP content block; reading `version` off
    /// the envelope is what broke "check for updates" in the field.
    #[test]
    fn the_listing_comes_out_of_the_content_block() {
        let listing = serde_json::json!({
            "tag": "v0.1.5",
            "version": "0.1.5",
            "assets": [{"name": "SHA256SUMS", "size": 7}],
            "sumsAsset": "SHA256SUMS"
        });
        let envelope = serde_json::json!({
            "content": [{"type": "text", "text": listing.to_string()}],
            "isError": false
        });
        assert_eq!(payload(&envelope).unwrap(), listing);

        // A protocol-2 stand-in returns the payload as the raw result.
        assert_eq!(payload(&listing).unwrap(), listing);

        // An envelope whose text is not JSON is a broken answer, not a listing.
        let broken = serde_json::json!({
            "content": [{"type": "text", "text": "ok"}],
            "isError": false
        });
        assert!(payload(&broken).is_none());

        // A blob result was already rewritten into structuredContent.
        let blob = serde_json::json!({
            "content": [{"type": "text", "text": "blob abc"}],
            "structuredContent": {"$blob": "abc", "size": 3}
        });
        assert!(payload(&blob).is_none());
    }

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(compare_versions("0.2.0", "0.1.9"), 1);
        assert_eq!(compare_versions("0.1.10", "0.1.9"), 1);
        assert_eq!(compare_versions("0.1.5", "0.1.5"), 0);
        assert_eq!(compare_versions("0.1.5", "0.2.0"), -1);
    }

    #[test]
    fn a_prerelease_is_never_an_update() {
        assert!(update_available("0.2.0", "0.1.9"));
        assert!(!update_available("0.2.0-rc1", "0.1.9"));
        assert!(!update_available("0.1.5", "0.1.5"));
        assert!(is_prerelease("0.2.0-rc1"));
        assert!(!is_prerelease("0.2.0"));
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

    /// A staged handover the way the store leaves one: the bytes are imported
    /// as a `sha256:` blob under `<temp>` and the caller only holds the id.
    #[cfg(feature = "http")]
    fn hand_over(config: &Config, bytes: &[u8]) -> (Handover, std::path::PathBuf) {
        let meta = crate::platform::storage::put(&config.paths, bytes).expect("put blob");
        let path = crate::platform::storage::locate(&config.paths, &meta.id).expect("blob path");
        let handover = take(&config.paths, &meta.id).expect("take blob");
        (handover, path)
    }

    /// A config that owns its whole tree and has a real install target, so
    /// `install_archive` gets past the development-build refusal.
    #[cfg(feature = "http")]
    fn upgrade_config(root: &std::path::Path) -> Config {
        let settings = crate::platform::settings::Settings {
            install_dir: Some(root.join("install")),
            ..Default::default()
        };
        Config::with_paths(settings, None, Paths::under(root))
    }

    /// #302: the handover is a one-shot input. It must not outlive the attempt,
    /// because the retention sweep runs hours later.
    #[cfg(feature = "http")]
    #[test]
    fn a_handover_is_released_when_it_ends() {
        let temp = crate::platform::test_support::TempDir::with_prefix("svcd-upgrade");
        let config = upgrade_config(temp.path());
        let (handover, path) = hand_over(&config, b"archive bytes");
        assert!(path.is_file());
        drop(handover);
        assert!(
            !path.exists(),
            "a consumed handover must not wait for the retention sweep"
        );
    }

    /// The reported failure: `tar could not unpack` left the 15 MB archive in
    /// `<temp>` for two hours.
    #[cfg(feature = "http")]
    #[test]
    fn an_unpack_that_fails_releases_the_archive() {
        let temp = crate::platform::test_support::TempDir::with_prefix("svcd-upgrade");
        let config = upgrade_config(temp.path());
        let asset = "svcd-0.9.0-x86_64-unknown-linux-gnu.tar.gz";
        let bytes = b"this is not a tar archive";
        let sums = format!("{}  {}\n", crate::manage::hex_sha256(bytes), asset);
        let (archive, archive_path) = hand_over(&config, bytes);
        let (sums, sums_path) = hand_over(&config, sums.as_bytes());
        let error = install_downloaded(&config, asset, archive, sums)
            .expect_err("tar cannot unpack a file that is not an archive");
        assert!(error.contains("tar could not unpack"), "{error}");
        assert!(!archive_path.exists(), "the archive stays in temp");
        assert!(!sums_path.exists(), "SHA256SUMS stays in temp");
    }

    /// A hash that does not match the archive ends the attempt before anything
    /// is written, and releases both handovers.
    #[cfg(feature = "http")]
    #[test]
    fn a_rejected_hash_releases_both_handovers() {
        let temp = crate::platform::test_support::TempDir::with_prefix("svcd-upgrade");
        let config = upgrade_config(temp.path());
        let asset = "svcd-0.9.0-x86_64-unknown-linux-gnu.tar.gz";
        let (archive, archive_path) = hand_over(&config, b"the archive");
        let wrong = format!("{}  {}\n", "0".repeat(64), asset);
        let (sums, sums_path) = hand_over(&config, wrong.as_bytes());
        let error = install_downloaded(&config, asset, archive, sums)
            .expect_err("a hash that does not match the archive is refused");
        assert!(error.contains("sha256 mismatch"), "{error}");
        assert!(!archive_path.exists());
        assert!(!sums_path.exists());
    }

    /// BusyBox `tar` answers `invalid tar magic` for a gzip stream read as a
    /// plain tar, which is what a device upgrade hit (#302).
    #[test]
    fn a_gzipped_archive_is_unpacked_with_explicit_gzip() {
        assert_eq!(tar_mode(&[0x1f, 0x8b, 0x08, 0x00]), "-xzf");
        assert_eq!(tar_mode(b"PK\x03\x04"), "-xf", "a Windows zip asset");
        assert_eq!(tar_mode(&[0u8; 512]), "-xf", "a plain tar");
        assert_eq!(tar_mode(b""), "-xf");
    }

    /// The unpack path end to end: a real gzip archive, the platform `tar`, and
    /// an install tree that receives the binary.
    #[cfg(feature = "http")]
    #[test]
    fn a_gzipped_release_unpacks_and_installs() {
        let temp = crate::platform::test_support::TempDir::with_prefix("svcd-upgrade");
        let config = upgrade_config(temp.path());
        let asset = "svcd-0.9.0-x86_64-unknown-linux-gnu.tar.gz";
        let bytes = gzipped_release(&config);
        let (archive, archive_path) = hand_over(&config, &bytes);
        let sums = format!("{}  {}\n", crate::manage::hex_sha256(&bytes), asset);
        let (sums, sums_path) = hand_over(&config, sums.as_bytes());
        install_downloaded(&config, asset, archive, sums).expect("a verified release installs");
        assert!(!archive_path.exists(), "the archive stays in temp");
        assert!(!sums_path.exists(), "SHA256SUMS stays in temp");
        let install_dir = config.settings.install_dir.clone().expect("install dir");
        let installed = install_dir.join(executable_names()[0].clone());
        assert!(installed.is_file(), "{} is missing", installed.display());
        for name in executable_names() {
            let path = install_dir.join(name);
            assert!(
                path.is_file(),
                "{} is missing after an upgrade",
                path.display()
            );
        }
        // #304: the four names are one inode, the way the release archive and
        // `svcd --install` both leave them (`stat -c '%h %s'` shows `4 <size>`).
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let links = fs::metadata(&installed).expect("metadata");
            assert_eq!(links.nlink(), 4, "the archive's hard links did not survive");
        }
        let work: Vec<_> = fs::read_dir(&config.paths.temp_dir)
            .expect("temp dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with("upgrade-"))
            .collect();
        assert!(work.is_empty(), "the unpack tree stays behind: {work:?}");
    }

    /// Build the archive the release workflow ships, with the same platform
    /// `tar` the upgrade path calls.
    #[cfg(feature = "http")]
    fn gzipped_release(config: &Config) -> Vec<u8> {
        let names = executable_names();
        let binary = names[0].clone();
        let staging = config.paths.temp_dir.join("staging");
        fs::create_dir_all(&staging).expect("staging");
        fs::write(staging.join(&binary), b"pretend binary").expect("binary");
        // The release ships the aliases as hard links to the binary
        // (`tools/package_release.py`), and `tar` restores them as links.
        for name in &names[1..] {
            link_or_copy(&staging.join(&binary), &staging.join(name));
        }
        let archive = config.paths.temp_dir.join("release.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .args(&names)
            .status()
            .expect("run tar");
        assert!(status.success(), "tar could not create the archive");
        let bytes = fs::read(&archive).expect("read archive");
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_file(&archive);
        bytes
    }

    /// #304: the archive ships the aliases as hard links, and an upgrade must
    /// land that shape. Copying every name left four independent binaries
    /// (~5.7 MB on the device's 30 MB `/data`) and no relationship to the
    /// archive at all.
    #[test]
    fn an_upgrade_keeps_the_aliases_sharing_the_binary() {
        let temp = crate::platform::test_support::TempDir::with_prefix("svcd-upgrade");
        let unpacked = temp.path().join("unpacked");
        let dir = temp.path().join("install");
        fs::create_dir_all(&unpacked).expect("unpacked");
        fs::create_dir_all(&dir).expect("install dir");
        let names = executable_names();
        let main = unpacked.join(&names[0]);
        fs::write(&main, b"binary").expect("archive binary");
        for name in &names[1..] {
            link_or_copy(&main, &unpacked.join(name));
        }
        // The tree the previous (copying) upgrade left behind: four names,
        // four copies.
        for name in &names {
            fs::write(dir.join(name), b"old binary").expect("previous install");
        }

        install_files(&dir, &unpacked).expect("install");

        for name in &names {
            let path = dir.join(name);
            assert!(path.is_file(), "{} is missing", path.display());
        }
        // Content written through the main binary shows through an alias only
        // when the two share an inode. Unix links; Windows copies, because a
        // running image cannot lose its last hard link.
        fs::write(dir.join(&names[0]), b"binary+marker").expect("marker");
        let through_alias = fs::read(dir.join(&names[1])).expect("read alias");
        #[cfg(unix)]
        assert_eq!(
            through_alias, b"binary+marker",
            "the alias is a second copy of the binary"
        );
        #[cfg(not(unix))]
        assert_eq!(
            through_alias, b"binary",
            "a copy must not follow the main binary"
        );
    }

    /// Some `tar` implementations expand the archive's hard links. The alias
    /// then carries its own bytes and is installed as shipped — never linked to
    /// a binary it may not match.
    #[test]
    fn an_alias_the_archive_left_unlinked_is_installed_as_shipped() {
        let temp = crate::platform::test_support::TempDir::with_prefix("svcd-upgrade");
        let unpacked = temp.path().join("unpacked");
        let dir = temp.path().join("install");
        fs::create_dir_all(&unpacked).expect("unpacked");
        fs::create_dir_all(&dir).expect("install dir");
        let names = executable_names();
        fs::write(unpacked.join(&names[0]), b"main").expect("archive binary");
        fs::write(unpacked.join(&names[1]), b"own bytes").expect("expanded alias");

        install_files(&dir, &unpacked).expect("install");

        let alias = fs::read(dir.join(&names[1])).expect("read alias");
        assert_eq!(alias, b"own bytes");
        let main = fs::read(dir.join(&names[0])).expect("read main");
        assert_eq!(main, b"main");
    }

    /// The release archive's own shape: three names hard-linked to the binary.
    fn link_or_copy(from: &Path, to: &Path) {
        if fs::hard_link(from, to).is_err() {
            fs::copy(from, to).expect("copy alias");
        }
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
