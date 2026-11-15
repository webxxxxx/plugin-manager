//! Host compatibility identity.
//!
//! A device exposes exactly one compatibility id (`device = sr6110`) in
//! `<config>/compat.conf`. A plugin may list the ids it supports:
//!
//! - no list  -> universal for this `platform`;
//! - a list   -> the host id must be one of them.
//!
//! The id is deliberately coarse: it identifies a *kernel/firmware
//! compatibility generation*, not just a model. Whenever a kernel module built
//! for the old id would break on the new firmware, the firmware must ship a new
//! id (`sr6110-v2`). Matching is exact string equality: no wildcards, no
//! prefixes, no version ranges.
//!
//! `svc compat` prints these facts; `svc compat init --device <id>` provisions
//! the marker on a device that was flashed before the marker existed.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::package;
use crate::paths;

/// The compatibility list a plugin declares in its signed manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatSpec {
    #[serde(default)]
    pub devices: Vec<String>,
}

impl CompatSpec {}

/// Facts about the machine svcd is running on. Serializable so `svc compat
/// --json` and `GET /api/v1/compat` share one shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostFacts {
    /// Compile-time target triple of this svcd binary.
    pub platform: String,
    /// Provisioned compatibility id; `None` when the marker is absent/invalid.
    pub device: Option<String>,
    /// `env` / `marker`, for field diagnostics.
    pub device_source: Option<String>,
    /// Running kernel release (`uname`), display/diagnostic only.
    pub kernel_release: Option<String>,
    pub svcd_version: String,
    /// Highest manifest schema this daemon understands.
    pub schema_max: u32,
    pub protocol_max: u32,
    /// Where the marker is read from / written to.
    pub marker_path: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No device list (universal) or the host id is listed.
    Compatible,
    /// A list exists, the host has an id, and the id is not in it.
    Incompatible(String),
    /// A list exists but the host id cannot be determined.
    Unknown(String),
}

pub fn marker_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SVCD_COMPAT_CONF") {
        return PathBuf::from(path);
    }
    paths::config_dir().join("compat.conf")
}

/// Read the marker: `None` when the file does not exist, `Some(id)` when it
/// parses. Problems are returned as warnings, never as hard errors: a broken
/// marker means "unknown", which fails closed for restricted packages.
fn read_marker(path: &Path) -> (Option<String>, Vec<String>) {
    let mut warnings = Vec::new();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (None, warnings);
        }
        Err(error) => {
            warnings.push(format!("cannot read {}: {}", path.display(), error));
            return (None, warnings);
        }
    };
    let mut device = None;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!(
                "{}:{}: expected key = value",
                path.display(),
                index + 1
            ));
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "device" => match validate_device_id(value) {
                Ok(()) => device = Some(value.to_string()),
                Err(error) => warnings.push(format!(
                    "{}:{}: invalid device id: {}",
                    path.display(),
                    index + 1,
                    error
                )),
            },
            other => warnings.push(format!(
                "{}:{}: ignoring unknown key {:?}",
                path.display(),
                index + 1,
                other
            )),
        }
    }
    (device, warnings)
}

pub fn detect() -> HostFacts {
    let path = marker_path();
    let (mut device, mut warnings) = read_marker(&path);
    let mut device_source = device.as_ref().map(|_| "marker".to_string());
    if let Some(value) = std::env::var_os("SVCD_DEVICE") {
        let value = value.to_string_lossy().trim().to_string();
        match validate_device_id(&value) {
            Ok(()) => {
                device = Some(value);
                device_source = Some("env".into());
            }
            Err(error) => warnings.push(format!("SVCD_DEVICE: {}", error)),
        }
    }
    if device.is_none() {
        warnings.push(format!(
            "device id is not provisioned; run `svc compat init --device <id>` ({})",
            path.display()
        ));
    }
    HostFacts {
        platform: package::target().to_string(),
        device,
        device_source,
        kernel_release: uname_release(),
        svcd_version: env!("CARGO_PKG_VERSION").to_string(),
        schema_max: package::MAX_SCHEMA,
        protocol_max: package::MAX_PROTOCOL,
        marker_path: path.to_string_lossy().to_string(),
        warnings,
    }
}

/// Evaluate a manifest's compatibility list against the host. Empty/absent
/// lists are universal; a host without an id yields `Unknown`, not
/// `Incompatible`.
pub fn evaluate(platform: &str, devices: &[String], host: &HostFacts) -> Verdict {
    if platform != host.platform {
        return Verdict::Incompatible(format!(
            "platform mismatch: package {}, host {}",
            platform, host.platform
        ));
    }
    if devices.is_empty() {
        return Verdict::Compatible;
    }
    let Some(device) = host.device.as_deref() else {
        return Verdict::Unknown(format!(
            "package is limited to devices [{}], but this host has no device id (svc compat init --device <id>)",
            devices.join(", ")
        ));
    };
    if devices.iter().any(|candidate| candidate == device) {
        Verdict::Compatible
    } else {
        Verdict::Incompatible(format!(
            "requires device [{}], host is {}",
            devices.join(", "),
            device
        ))
    }
}

pub fn verdict_reason(verdict: Verdict) -> Option<String> {
    match verdict {
        Verdict::Compatible => None,
        Verdict::Incompatible(reason) | Verdict::Unknown(reason) => Some(reason),
    }
}

pub fn validate_device_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 {
        return Err("device id must be 1..=64 characters".into());
    }
    let mut chars = id.chars();
    let first = chars.next().expect("checked non-empty");
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err("device id must start with a lowercase letter or digit".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
    {
        return Err("device id may only contain [a-z0-9._-]".into());
    }
    Ok(())
}

/// Write (or replace) the marker. Atomic where the platform allows it; the
/// daemon never writes this file, only the operator does.
pub fn write_device(path: &Path, device: &str) -> Result<(), String> {
    validate_device_id(device)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {}", parent.display(), error))?;
    }
    let text = format!(
        "# svcd host compatibility.\n\
         # One id per kernel/firmware compatibility generation: if a kernel module\n\
         # built for the old id breaks on new firmware, the firmware must ship a\n\
         # new id. Plugins that list ids are rejected elsewhere.\n\
         device = {}\n",
        device
    );
    let temporary = path.with_extension("conf.tmp");
    fs::write(&temporary, &text)
        .map_err(|error| format!("write {}: {}", temporary.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644));
    }
    if fs::rename(&temporary, path).is_err() {
        fs::write(path, &text).map_err(|error| format!("write {}: {}", path.display(), error))?;
        let _ = fs::remove_file(&temporary);
    }
    Ok(())
}

#[cfg(unix)]
fn uname_release() -> Option<String> {
    let mut buffer: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut buffer) } != 0 {
        return None;
    }
    let release = unsafe { std::ffi::CStr::from_ptr(buffer.release.as_ptr()) };
    release
        .to_str()
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(unix))]
fn uname_release() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(device: Option<&str>) -> HostFacts {
        HostFacts {
            platform: "aarch64-unknown-linux-musl".into(),
            device: device.map(str::to_string),
            device_source: device.map(|_| "marker".to_string()),
            kernel_release: Some("5.4.196".into()),
            svcd_version: "0.1.1".into(),
            schema_max: 3,
            protocol_max: 2,
            marker_path: "/tmp/compat.conf".into(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn an_empty_list_is_universal() {
        let host = host(None);
        assert_eq!(
            evaluate("aarch64-unknown-linux-musl", &[], &host),
            Verdict::Compatible
        );
    }

    #[test]
    fn a_list_matches_only_the_exact_device_id() {
        let host = host(Some("sr6110"));
        let listed = vec!["sr6110".to_string()];
        assert_eq!(
            evaluate("aarch64-unknown-linux-musl", &listed, &host),
            Verdict::Compatible
        );
        let other = vec!["sr1010".to_string()];
        assert!(matches!(
            evaluate("aarch64-unknown-linux-musl", &other, &host),
            Verdict::Incompatible(_)
        ));
        let prefixed = vec!["sr6110-v2".to_string()];
        assert!(matches!(
            evaluate("aarch64-unknown-linux-musl", &prefixed, &host),
            Verdict::Incompatible(_)
        ));
    }

    #[test]
    fn a_list_without_a_host_id_is_unknown_not_incompatible() {
        let host = host(None);
        let listed = vec!["sr6110".to_string()];
        assert!(matches!(
            evaluate("aarch64-unknown-linux-musl", &listed, &host),
            Verdict::Unknown(_)
        ));
    }

    #[test]
    fn platform_mismatch_wins_over_everything() {
        let host = host(Some("sr6110"));
        assert!(matches!(
            evaluate("x86_64-unknown-linux-musl", &[], &host),
            Verdict::Incompatible(_)
        ));
    }

    #[test]
    fn device_ids_follow_the_documented_shape() {
        for good in ["sr6110", "sr6110-v2", "ax3000.pro_plus", "r1"] {
            validate_device_id(good).expect(good);
        }
        for bad in ["", "SR6110", "-sr", "sr 6110", "sr/6110", "sr6110!"] {
            assert!(validate_device_id(bad).is_err(), "{bad}");
        }
        assert!(validate_device_id(&"a".repeat(65)).is_err());
    }

    #[test]
    fn the_marker_round_trips_and_ignores_unknown_keys() {
        let root = std::env::temp_dir().join(format!(
            "svcd-compat-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let path = root.join("compat.conf");
        write_device(&path, "sr6110-v2").expect("write");
        let (device, warnings) = read_marker(&path);
        assert_eq!(device.as_deref(), Some("sr6110-v2"));
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::write(&path, "device = sr6110\nfuture_key = x\n").expect("marker");
        let (device, warnings) = read_marker(&path);
        assert_eq!(device.as_deref(), Some("sr6110"));
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        fs::write(&path, "device = WRONG\n").expect("marker");
        let (device, warnings) = read_marker(&path);
        assert_eq!(device, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let _ = fs::remove_dir_all(root);
    }
}
