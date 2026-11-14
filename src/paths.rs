use std::path::PathBuf;

use crate::settings;

/// Mode 1: a non-root process uses its own profile. Root (or an explicit
/// `SVCD_USER=0`) keeps the system-wide profile.
pub fn user_mode() -> bool {
    match std::env::var("SVCD_USER").ok().as_deref() {
        Some("0") => false,
        Some("1") => true,
        _ => !crate::install::is_privileged(),
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(not(windows))]
fn xdg_dir(variable: &str, fallback: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os(variable) {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("svcd");
        }
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(fallback)
        .join("svcd")
}

pub fn user_config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(dir) = std::env::var_os("APPDATA") {
            return PathBuf::from(dir).join("svcd");
        }
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".svcd")
    }
    #[cfg(not(windows))]
    {
        xdg_dir("XDG_CONFIG_HOME", ".config")
    }
}

pub fn user_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(dir).join("svcd");
        }
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".svcd")
    }
    #[cfg(not(windows))]
    {
        xdg_dir("XDG_DATA_HOME", ".local/share")
    }
}

pub fn user_logs_dir() -> PathBuf {
    #[cfg(windows)]
    {
        user_data_dir().join("logs")
    }
    #[cfg(not(windows))]
    {
        xdg_dir("XDG_STATE_HOME", ".local/state").join("logs")
    }
}

pub(crate) fn default_config_dir() -> PathBuf {
    if user_mode() {
        user_config_dir()
    } else {
        platform_config_dir()
    }
}

/// The configuration directory: `svcd.conf`, `trust.d`, `lan.json` and the
/// local control token. `SVCD_CONFIG_DIR` overrides it for tests; otherwise
/// `config_dir` in the loaded `svcd.conf` wins over the platform default.
/// Production defaults are `/etc/svcd`, `/Library/Application Support/svcd/config`
/// and `%USERPROFILE%\.svcd`; a non-root process defaults to its user profile.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(settings) = settings::current() {
        if let Some(dir) = &settings.config_dir {
            return dir.clone();
        }
    }
    default_config_dir()
}

pub fn services_dir() -> PathBuf {
    settings::current()
        .ok()
        .and_then(|settings| settings.services_dir.clone())
        .unwrap_or_else(|| config_dir().join("services.d"))
}

/// Transient plugin data. `SVCD_TEMP_DIR` overrides it for tests/plugins;
/// `temp_dir` in `svcd.conf` wins over the default `<data>/temp`.
pub fn temp_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_TEMP_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(settings) = settings::current() {
        if let Some(dir) = &settings.temp_dir {
            return dir.clone();
        }
    }
    data_dir().join("temp")
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
/// non-root development; `install_dir` moves it under the installation root
/// for read-only-rootfs hosts. Production paths are `/var/log/svcd`,
/// `/Library/Logs/svcd` and `%ProgramData%\svcd\logs`.
pub fn logs_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_LOG_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(settings) = settings::current() {
        if let Some(dir) = settings.install_logs_dir() {
            return dir;
        }
    }
    if user_mode() {
        return user_logs_dir();
    }
    platform_logs_dir()
}

/// The platform data directory. `SVCD_DATA_DIR` overrides it so tests and
/// non-root development runs can use an isolated tree; `install_dir` moves it
/// under the installation root when `/var` is not persistent. Production paths
/// are `/var/lib/svcd`, `/Library/Application Support/svcd` and
/// `%ProgramData%\svcd`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(settings) = settings::current() {
        if let Some(dir) = settings.install_data_dir() {
            return dir;
        }
    }
    if user_mode() {
        return user_data_dir();
    }
    platform_data_dir()
}

#[cfg(target_os = "windows")]
pub(crate) fn platform_config_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Default"));
    home.join(".svcd")
}

#[cfg(target_os = "macos")]
pub(crate) fn platform_config_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/svcd/config")
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn platform_config_dir() -> PathBuf {
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
