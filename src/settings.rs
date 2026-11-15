//! Runtime settings loaded from the `svcd.conf` chosen by `install`.
//!
//! The file is a small `key = value` document. The install layer resolves
//! which file to use (explicit install path, program directory, config
//! directory, or a generated default); this module only parses and caches it.
//!
//! `SVCD_BIND` overrides the `bind` key for tests and ad-hoc runs; explicit
//! paths in `svcd.conf` win over the platform defaults.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static SETTINGS: OnceLock<Result<Settings, String>> = OnceLock::new();
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// HTTP listener address. Default: `127.0.0.1:8787`.
    pub bind: Option<SocketAddr>,
    pub config_dir: Option<PathBuf>,
    pub plugins_dir: Option<PathBuf>,
    pub services_dir: Option<PathBuf>,
    pub auth_passwd: Option<String>,
    /// Root for transient plugin data (`SVCD_TEMP_DIR`). Defaults to
    /// `<data>/temp`.
    pub temp_dir: Option<PathBuf>,
    /// Files under `temp_dir` older than this are removed. Default: 2 hours.
    pub temp_retention_secs: Option<u64>,
    /// How often the cleanup thread scans `temp_dir`. `0` disables cleanup.
    pub temp_cleanup_interval_secs: Option<u64>,
    /// Relocated installation root for hosts whose `/usr` is read-only and
    /// whose writable tree is limited (SR6110: only `/etc` persists). When
    /// set, `--install` puts the binary, aliases and `svcd_daemon.sh` here;
    /// `data` and `logs` default to `<install_dir>/data` and `<install_dir>/logs`.
    pub install_dir: Option<PathBuf>,
    /// Allow installs to fall back to accepting a package whose signature
    /// cannot be verified. Default `false`: unsigned packages are rejected.
    pub allow_insecure_install: bool,
}

impl Settings {
    /// Runtime data directory implied by `install_dir`, if that key is set.
    pub fn install_data_dir(&self) -> Option<PathBuf> {
        self.install_dir.as_ref().map(|dir| dir.join("data"))
    }

    /// Runtime log directory implied by `install_dir`, if that key is set.
    pub fn install_logs_dir(&self) -> Option<PathBuf> {
        self.install_dir.as_ref().map(|dir| dir.join("logs"))
    }

    pub fn temp_retention(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.temp_retention_secs.unwrap_or(2 * 60 * 60))
    }

    pub fn temp_cleanup_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.temp_cleanup_interval_secs.unwrap_or(5 * 60))
    }
}

/// The settings for this process. Loaded once from `SVCD_CONFIG_DIR` when it is
/// set, otherwise the program directory first and the platform config
/// directory second. An explicitly initialized service config wins over both.
pub fn current() -> Result<&'static Settings, String> {
    SETTINGS
        .get_or_init(|| {
            if let Some(dir) = std::env::var_os("SVCD_CONFIG_DIR") {
                return load(&PathBuf::from(dir));
            }
            let program_conf = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|parent| parent.join("svcd.conf")));
            if let Some(path) = program_conf {
                if path.is_file() {
                    return load_from(&path);
                }
            }
            load(&crate::paths::default_config_dir())
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The file this process loaded. `svc auth passwd` updates this exact file.
pub fn path() -> Option<&'static Path> {
    CONFIG_PATH.get().map(PathBuf::as_path)
}

/// Whether the operator enabled unsigned installs in `svcd.conf`. This is the
/// only switch: CLI, HTTP and the console have no per-install insecure option.
pub fn allow_insecure_install() -> bool {
    current()
        .map(|settings| settings.allow_insecure_install)
        .unwrap_or(false)
}

/// Load the config file chosen by `install::ensure_conf` into the process cache.
pub fn init_from_path(path: &Path) -> Result<(), String> {
    if let Some(existing) = SETTINGS.get() {
        return existing.as_ref().map(|_| ()).map_err(Clone::clone);
    }
    if !path.is_file() {
        return Err(format!("config not found: {}", path.display()));
    }
    match load_from(path) {
        Ok(settings) => {
            let _ = SETTINGS.set(Ok(settings));
            Ok(())
        }
        Err(error) => {
            let _ = SETTINGS.set(Err(error.clone()));
            Err(error)
        }
    }
}

fn load(config_dir: &Path) -> Result<Settings, String> {
    load_from(&config_dir.join("svcd.conf"))
}

fn load_from(path: &Path) -> Result<Settings, String> {
    let _ = CONFIG_PATH.set(path.to_path_buf());
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default())
        }
        Err(error) => return Err(format!("read {}: {}", path.display(), error)),
    };
    parse(&text, path.parent().unwrap_or(Path::new(".")))
        .map_err(|error| format!("{}: {}", path.display(), error))
}

pub fn parse(text: &str, base: &Path) -> Result<Settings, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut settings = Settings::default();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: expected key = value", index + 1))?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            return Err(format!("line {}: empty key", index + 1));
        }
        if value.is_empty() {
            return Err(format!("line {}: empty value for {}", index + 1, key));
        }
        match key {
            "bind" => {
                settings.bind = Some(
                    value
                        .parse::<SocketAddr>()
                        .map_err(|_| format!("line {}: invalid bind {:?}", index + 1, value))?,
                );
            }
            "allow_insecure_install" => {
                settings.allow_insecure_install = parse_bool(value).ok_or_else(|| {
                    format!(
                        "line {}: invalid allow_insecure_install {:?}",
                        index + 1,
                        value
                    )
                })?;
            }
            "config_dir" => settings.config_dir = Some(resolve(base, value)),
            "plugins_dir" => settings.plugins_dir = Some(resolve(base, value)),
            "services_dir" => settings.services_dir = Some(resolve(base, value)),
            "install_dir" => settings.install_dir = Some(resolve(base, value)),
            "auth_passwd" => settings.auth_passwd = Some(value.to_string()),
            "temp_dir" => settings.temp_dir = Some(resolve(base, value)),
            "temp_retention_secs" => {
                let seconds = value.parse::<u64>().map_err(|_| {
                    format!(
                        "line {}: invalid temp_retention_secs {:?}",
                        index + 1,
                        value
                    )
                })?;
                if seconds == 0 {
                    return Err(format!(
                        "line {}: temp_retention_secs must be greater than zero",
                        index + 1
                    ));
                }
                settings.temp_retention_secs = Some(seconds);
            }
            "temp_cleanup_interval_secs" => {
                settings.temp_cleanup_interval_secs = Some(value.parse::<u64>().map_err(|_| {
                    format!(
                        "line {}: invalid temp_cleanup_interval_secs {:?}",
                        index + 1,
                        value
                    )
                })?);
            }
            other => return Err(format!("line {}: unknown key {:?}", index + 1, other)),
        }
    }
    Ok(settings)
}

fn resolve(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Some(true),
        "false" | "0" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Replace one key in an existing conf, preserving comments and other keys.
/// Used by `svc auth passwd` to write `auth_passwd` without a restart.
pub fn update_value(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {}", path.display(), error))?;
    let mut output = String::with_capacity(text.len() + value.len() + 16);
    let mut replaced = false;
    for line in text.lines() {
        let trimmed = line.trim();
        let matches_key = !trimmed.starts_with('#')
            && trimmed
                .split_once('=')
                .map(|(left, _)| left.trim() == key)
                .unwrap_or(false);
        if matches_key {
            if !replaced {
                output.push_str(&format!("{} = {}\n", key, value));
                replaced = true;
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    if !replaced {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&format!("{} = {}\n", key, value));
    }
    fs::write(path, output).map_err(|error| format!("write {}: {}", path.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_keys_and_resolves_relative_paths() {
        let settings = parse(
            "\u{feff}# svcd runtime\nbind = 0.0.0.0:8787\nallow_insecure_install = on\nconfig_dir = /srv/svcd\nplugins_dir = plugins\nservices_dir = /var/lib/svcd/services.d\ninstall_dir = /etc/sr/svcd\nauth_passwd = pbkdf2-sha256$10000$c2FsdA$aGFzaA\ntemp_dir = /var/tmp/svcd\ntemp_retention_secs = 3600\ntemp_cleanup_interval_secs = 60\n",
            Path::new("/etc/svcd"),
        )
        .expect("parse");
        assert_eq!(
            settings.bind,
            Some("0.0.0.0:8787".parse::<SocketAddr>().expect("bind"))
        );
        assert!(settings.allow_insecure_install);
        assert_eq!(settings.config_dir, Some(PathBuf::from("/srv/svcd")));
        assert_eq!(
            settings.plugins_dir,
            Some(PathBuf::from("/etc/svcd/plugins"))
        );
        assert_eq!(
            settings.services_dir,
            Some(PathBuf::from("/var/lib/svcd/services.d"))
        );
        assert_eq!(settings.install_dir, Some(PathBuf::from("/etc/sr/svcd")));
        assert_eq!(
            settings.auth_passwd.as_deref(),
            Some("pbkdf2-sha256$10000$c2FsdA$aGFzaA")
        );
        assert_eq!(settings.temp_dir, Some(PathBuf::from("/var/tmp/svcd")));
        assert_eq!(settings.temp_retention_secs, Some(3600));
        assert_eq!(settings.temp_cleanup_interval_secs, Some(60));
        assert_eq!(settings.temp_retention().as_secs(), 3600);
        assert_eq!(settings.temp_cleanup_interval().as_secs(), 60);
    }

    #[test]
    fn blank_lines_and_duplicate_keys_are_tolerated() {
        let settings = parse(
            "bind = 127.0.0.1:1\n\n   \n# comment\nbind = 127.0.0.1:2\n",
            Path::new("/etc/svcd"),
        )
        .expect("parse");
        assert_eq!(
            settings.bind,
            Some("127.0.0.1:2".parse::<SocketAddr>().expect("bind")),
            "the last declaration wins"
        );
    }

    #[test]
    fn update_value_collapses_duplicate_keys() {
        let directory = std::env::temp_dir().join(format!("svcd-settings-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("temp dir");
        let path = directory.join("svcd.conf");
        fs::write(
            &path,
            "bind = 127.0.0.1:1\n\nbind = 127.0.0.1:2\nallow_insecure_install = yes\n",
        )
        .expect("write conf");
        update_value(&path, "bind", "127.0.0.1:3").expect("update");
        let text = fs::read_to_string(&path).expect("read conf");
        assert_eq!(text.matches("bind").count(), 1, "{text}");
        assert!(text.contains("bind = 127.0.0.1:3"), "{text}");
        assert!(text.contains("allow_insecure_install = yes"), "{text}");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn temp_defaults_are_two_hours_and_five_minutes() {
        let settings = Settings::default();
        assert_eq!(settings.temp_retention().as_secs(), 2 * 60 * 60);
        assert_eq!(settings.temp_cleanup_interval().as_secs(), 5 * 60);
        assert!(!settings.allow_insecure_install);
    }

    #[test]
    fn relative_install_dir_resolves_against_the_conf_directory() {
        let settings = parse("install_dir = sr/svcd\n", Path::new("/etc/svcd")).expect("parse");
        assert_eq!(
            settings.install_dir,
            Some(PathBuf::from("/etc/svcd/sr/svcd"))
        );
    }

    #[test]
    fn install_dir_implies_data_and_log_directories() {
        let settings =
            parse("install_dir = /etc/sr/svcd\n", Path::new("/etc/svcd")).expect("parse");
        assert_eq!(
            settings.install_data_dir(),
            Some(PathBuf::from("/etc/sr/svcd/data"))
        );
        assert_eq!(
            settings.install_logs_dir(),
            Some(PathBuf::from("/etc/sr/svcd/logs"))
        );
        assert_eq!(Settings::default().install_data_dir(), None);
        assert_eq!(Settings::default().install_logs_dir(), None);
    }

    #[test]
    fn update_value_replaces_or_appends_one_key() {
        let directory = std::env::temp_dir().join(format!(
            "svcd-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("temp dir");
        let path = directory.join("svcd.conf");
        fs::write(&path, "# keep\nbind = 127.0.0.1:8787\nauth_passwd = old\n").expect("write conf");

        update_value(&path, "auth_passwd", "new$hash").expect("replace");
        let text = fs::read_to_string(&path).expect("read");
        assert_eq!(
            text,
            "# keep\nbind = 127.0.0.1:8787\nauth_passwd = new$hash\n"
        );

        update_value(&path, "bind", "0.0.0.0:8787").expect("append");
        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains("auth_passwd = new$hash"));
        assert!(text.contains("bind = 0.0.0.0:8787"), "{text}");

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn false_spellings_are_false() {
        for value in ["false", "0", "off", "no", "FALSE"] {
            let settings = parse(
                &format!("allow_insecure_install = {value}\n"),
                Path::new("/etc/svcd"),
            )
            .expect("parse");
            assert!(!settings.allow_insecure_install, "{value}");
        }
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let settings = parse(
            "\n# comment\n   \nbind = 127.0.0.1:0\n",
            Path::new("/etc/svcd"),
        )
        .expect("parse");
        assert_eq!(
            settings.bind,
            Some("127.0.0.1:0".parse::<SocketAddr>().expect("bind"))
        );
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        for text in [
            "unknown = 1\n",
            "bind = 70000\n",
            "bind = nope\n",
            "bind = 127.0.0.1\n",
            "allow_insecure_install = maybe\n",
            "bind 127.0.0.1:8787\n",
            "= 1\n",
        ] {
            assert!(parse(text, Path::new("/etc/svcd")).is_err(), "{text:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_absolute_paths_are_not_joined_again() {
        let settings =
            parse("plugins_dir = C:\\svcd\\plugins\n", Path::new("C:\\config")).expect("parse");
        assert_eq!(
            settings.plugins_dir,
            Some(PathBuf::from(r"C:\svcd\plugins"))
        );
    }
}
