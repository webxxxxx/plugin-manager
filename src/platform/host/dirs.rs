//! Host directory facts: where this config/data/logs live by default.
//!
//! Moved out of `paths` (#138): `settings` needs the *default* config
//! directory while it is still deciding which `svcd.conf` to load, so the
//! platform has to answer that question without going through `paths` — which
//! consults the loaded settings. `paths` re-exports these for its callers.

use std::path::PathBuf;

/// Mode 1: a non-root process uses its own profile. Root (or an explicit
/// `SVCD_USER=0`) keeps the system-wide profile.
pub fn user_mode() -> bool {
    match std::env::var("SVCD_USER").ok().as_deref() {
        Some("0") => false,
        Some("1") => true,
        _ => !crate::platform::is_privileged(),
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

#[cfg(target_os = "windows")]
pub(crate) fn platform_config_dir() -> PathBuf {
    // System mode is machine-wide: the SCM service runs as LocalSystem, whose
    // `%USERPROFILE%` is a different, ACL-private profile from the admin's.
    // A per-profile config path would give the service and the elevated CLI
    // different control-pipe names (the pipe name hashes the config path) and
    // would make `trust.d` / `auth_passwd` invisible to one of them.
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("svcd").join("config")
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
pub(crate) fn platform_logs_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("svcd").join("logs")
}

#[cfg(target_os = "macos")]
pub(crate) fn platform_logs_dir() -> PathBuf {
    PathBuf::from("/Library/Logs/svcd")
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn platform_logs_dir() -> PathBuf {
    PathBuf::from("/var/log/svcd")
}

#[cfg(target_os = "windows")]
pub(crate) fn platform_data_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("svcd")
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;

    #[test]
    fn system_config_is_machine_wide_and_not_in_a_user_profile() {
        let machine = platform_config_dir();
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        assert!(machine.starts_with(base.join("svcd")), "{machine:?}");
        assert!(
            !machine.starts_with(user_config_dir()),
            "system mode must not share the user-mode config: {machine:?}"
        );
        if let Some(home) = home_dir() {
            assert!(!machine.starts_with(home), "{machine:?}");
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn platform_data_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/svcd")
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn platform_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/svcd")
}
