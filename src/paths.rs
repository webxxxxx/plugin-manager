use std::path::PathBuf;

use crate::platform::host::dirs;
use crate::settings;

pub use crate::platform::host::dirs::user_mode;
pub(crate) use crate::platform::host::dirs::platform_config_dir;

/// The configuration directory: `svcd.conf`, `trust.d` and the local control
/// token. `SVCD_CONFIG_DIR` overrides it for tests; otherwise `config_dir` in
/// the loaded `svcd.conf` wins over the platform default.
/// Production defaults are `/etc/svcd`, `/Library/Application Support/svcd/config`
/// and `%ProgramData%\svcd\config`; a non-root process defaults to its user
/// profile (`%APPDATA%\svcd` on Windows).
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(settings) = settings::current() {
        if let Some(dir) = &settings.config_dir {
            return dir.clone();
        }
    }
    dirs::default_config_dir()
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

/// Per-service runtime records (`<data>/run/<id>.json`) that let a restarted
/// daemon reconnect to the processes that survived it. `SVCD_RUN_DIR` overrides
/// it for tests.
pub fn run_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_RUN_DIR") {
        return PathBuf::from(dir);
    }
    data_dir().join("run")
}

/// Installed plugins: `<data>/plugins/<id>/<version>/`, or `plugins_dir` from
/// `svcd.conf` when configured.
pub fn plugins_dir() -> PathBuf {
    // Unit tests share one process and may have loaded settings from another
    // test's config. The override is thread-local so one test's temp tree
    // cannot redirect a later test on another thread.
    #[cfg(test)]
    if let Some(dir) = TEST_PLUGINS_DIR.with(|slot| slot.borrow().clone()) {
        return dir;
    }
    settings::current()
        .ok()
        .and_then(|settings| settings.plugins_dir.clone())
        .unwrap_or_else(|| data_dir().join("plugins"))
}

#[cfg(test)]
thread_local! {
    static TEST_PLUGINS_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point the current test thread at its own plugin tree.
#[cfg(test)]
pub(crate) fn set_test_plugins_dir(dir: PathBuf) {
    TEST_PLUGINS_DIR.with(|slot| *slot.borrow_mut() = Some(dir));
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
        if let Some(dir) = &settings.logs_dir {
            return dir.clone();
        }
        if let Some(dir) = settings.install_logs_dir() {
            return dir;
        }
    }
    if user_mode() {
        return dirs::user_logs_dir();
    }
    dirs::platform_logs_dir()
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
        return dirs::user_data_dir();
    }
    dirs::platform_data_dir()
}
