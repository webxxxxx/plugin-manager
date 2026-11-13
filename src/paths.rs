use std::path::PathBuf;

use crate::settings;

/// The platform configuration directory. `SVCD_CONFIG_DIR` overrides it so tests
/// and non-root development runs can use an isolated tree; production paths are
/// `/etc/svcd`, `/Library/Application Support/svcd/config` and
/// `%ProgramData%\svcd\config`.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    platform_config_dir()
}

pub fn services_dir() -> PathBuf {
    settings::current()
        .ok()
        .and_then(|settings| settings.services_dir.clone())
        .unwrap_or_else(|| config_dir().join("services.d"))
}

/// Installed plugins: `<data>/plugins/<id>/<version>/`, or the
/// `plugins_dir` from `svcd.conf`.
pub fn plugins_dir() -> PathBuf {
    settings::current()
        .ok()
        .and_then(|settings| settings.plugins_dir.clone())
        .unwrap_or_else(|| data_dir().join("plugins"))
}

/// Ed25519 trust roots: `<config>/trust.d/<keyId>.pub` (PEM SPKI).
pub fn trust_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_TRUST_DIR") {
        return PathBuf::from(dir);
    }
    config_dir().join("trust.d")
}

/// The platform log directory. `SVCD_LOG_DIR` overrides it for tests and
/// non-root development; production paths are `/var/log/svcd`,
/// `/Library/Logs/svcd` and `%ProgramData%\svcd\logs`.
pub fn logs_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_LOG_DIR") {
        return PathBuf::from(dir);
    }
    platform_logs_dir()
}

/// The platform data directory. `SVCD_DATA_DIR` overrides it so tests and
/// non-root development runs can use an isolated tree; production paths are
/// `/var/lib/svcd`, `/Library/Application Support/svcd` and
/// `%ProgramData%\svcd`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_DATA_DIR") {
        return PathBuf::from(dir);
    }
    platform_data_dir()
}

#[cfg(target_os = "windows")]
fn platform_config_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("svcd").join("config")
}

#[cfg(target_os = "macos")]
fn platform_config_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/svcd/config")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_config_dir() -> PathBuf {
    PathBuf::from("/etc/svcd")
}

#[cfg(target_os = "windows")]
fn platform_logs_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("svcd").join("logs")
}

#[cfg(target_os = "macos")]
fn platform_logs_dir() -> PathBuf {
    PathBuf::from("/Library/Logs/svcd")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_logs_dir() -> PathBuf {
    PathBuf::from("/var/log/svcd")
}

#[cfg(target_os = "windows")]
fn platform_data_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("svcd")
}

#[cfg(target_os = "macos")]
fn platform_data_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/svcd")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/svcd")
}
