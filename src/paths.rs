use std::path::PathBuf;

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
    config_dir().join("services.d")
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
