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
    /// How often the supervisor scans `temp_dir`. `0` disables cleanup.
    pub temp_cleanup_interval_secs: Option<u64>,
    /// Per-service log directory. Empty/absent uses the platform or
    /// `install_dir/logs` default.
    pub logs_dir: Option<PathBuf>,
    /// Rotation threshold for one log file. Default is chosen by `MEM_LEVEL`.
    pub log_max_bytes: Option<u64>,
    /// Number of rotated files to keep (1 = only the current file).
    pub log_rotations: Option<usize>,
    /// `false` keeps logs on stderr/syslog only. Default: true.
    pub log_to_file: Option<bool>,
    /// Explicit thread stack sizes. `MEM_LEVEL` supplies the defaults.
    pub request_stack_bytes: Option<usize>,
    pub io_stack_bytes: Option<usize>,
    /// Worker threads for the blocking face pool. Default: 2, clamped 2..8.
    pub blocking_threads: Option<usize>,
    /// Runtime overrides/caps for the HTTP surface. All values are bytes or
    /// counts; `limits` clamps them to a safe range.
    pub max_body_bytes: Option<u64>,
    pub max_json_body_bytes: Option<u64>,
    pub max_complete_body_bytes: Option<u64>,
    pub max_header_bytes: Option<usize>,
    pub max_streams: Option<usize>,
    pub stream_buffer_bytes: Option<usize>,
    pub max_connections: Option<usize>,
    pub max_connections_per_ip: Option<usize>,
    pub max_subscriptions: Option<usize>,
    pub max_subscribers_per_plugin: Option<usize>,
    pub http_idle_timeout_secs: Option<u64>,
    pub max_package_bytes: Option<u64>,
    pub transfer_timeout_secs: Option<u64>,
    /// Bound for one management route's control-channel round trip.
    /// Default: 30, clamped 1..300. Requires a daemon restart.
    pub control_timeout_secs: Option<u64>,
    /// 默认调用超时（毫秒）；0/缺失时由 limits 取默认值。
    pub default_timeout_ms: Option<u64>,
    /// 调用超时上限（毫秒）；必须 >= default_timeout。
    pub max_timeout_ms: Option<u64>,
    pub ws_max_message_bytes: Option<usize>,
    pub ws_inflight_bytes: Option<usize>,
    pub ws_inflight_requests: Option<usize>,
    pub ws_buffer_shrink_bytes: Option<usize>,
    pub ws_raw_enabled: Option<bool>,
    pub ws_raw_max_bytes: Option<usize>,
    pub ws_raw_queue: Option<usize>,
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
            "logs_dir" => settings.logs_dir = Some(resolve(base, value)),
            "log_max_bytes" => {
                settings.log_max_bytes = Some(parse_u64(value, "log_max_bytes", index)?)
            }
            "log_rotations" => {
                settings.log_rotations = Some(parse_usize(value, "log_rotations", index)?)
            }
            "log_to_file" => {
                settings.log_to_file = Some(parse_bool(value).ok_or_else(|| {
                    format!("line {}: invalid log_to_file {:?}", index + 1, value)
                })?)
            }
            "request_stack_bytes" => {
                settings.request_stack_bytes =
                    Some(parse_usize(value, "request_stack_bytes", index)?)
            }
            "io_stack_bytes" => {
                settings.io_stack_bytes = Some(parse_usize(value, "io_stack_bytes", index)?)
            }
            "blocking_threads" => {
                settings.blocking_threads = Some(parse_usize(value, "blocking_threads", index)?)
            }
            "max_body_bytes" => {
                settings.max_body_bytes = Some(parse_u64(value, "max_body_bytes", index)?)
            }
            "max_json_body_bytes" => {
                settings.max_json_body_bytes = Some(parse_u64(value, "max_json_body_bytes", index)?)
            }
            "max_complete_body_bytes" => {
                settings.max_complete_body_bytes =
                    Some(parse_u64(value, "max_complete_body_bytes", index)?)
            }
            "max_header_bytes" => {
                settings.max_header_bytes = Some(parse_usize(value, "max_header_bytes", index)?)
            }
            "max_streams" => settings.max_streams = Some(parse_usize(value, "max_streams", index)?),
            "stream_buffer_bytes" => {
                settings.stream_buffer_bytes =
                    Some(parse_usize(value, "stream_buffer_bytes", index)?)
            }
            "max_connections" => {
                settings.max_connections = Some(parse_usize(value, "max_connections", index)?)
            }
            "max_connections_per_ip" => {
                settings.max_connections_per_ip =
                    Some(parse_usize(value, "max_connections_per_ip", index)?)
            }
            "max_subscriptions" => {
                settings.max_subscriptions = Some(parse_usize(value, "max_subscriptions", index)?)
            }
            "max_subscribers_per_plugin" => {
                settings.max_subscribers_per_plugin =
                    Some(parse_usize(value, "max_subscribers_per_plugin", index)?)
            }
            "http_idle_timeout_secs" => {
                settings.http_idle_timeout_secs =
                    Some(parse_u64(value, "http_idle_timeout_secs", index)?)
            }
            "max_package_bytes" => {
                settings.max_package_bytes = Some(parse_u64(value, "max_package_bytes", index)?)
            }
            "transfer_timeout_secs" => {
                settings.transfer_timeout_secs =
                    Some(parse_u64(value, "transfer_timeout_secs", index)?)
            }
            "control_timeout_secs" => {
                settings.control_timeout_secs =
                    Some(parse_u64(value, "control_timeout_secs", index)?)
            }
            "default_timeout" => {
                settings.default_timeout_ms =
                    Some(parse_u64(value, "default_timeout", index)?)
            }
            "max_timeout" => {
                settings.max_timeout_ms = Some(parse_u64(value, "max_timeout", index)?)
            }
            "ws_max_message_bytes" => {
                settings.ws_max_message_bytes =
                    Some(parse_usize(value, "ws_max_message_bytes", index)?)
            }
            "ws_inflight_bytes" => {
                settings.ws_inflight_bytes = Some(parse_usize(value, "ws_inflight_bytes", index)?)
            }
            "ws_inflight_requests" => {
                settings.ws_inflight_requests =
                    Some(parse_usize(value, "ws_inflight_requests", index)?)
            }
            "ws_buffer_shrink_bytes" => {
                settings.ws_buffer_shrink_bytes =
                    Some(parse_usize(value, "ws_buffer_shrink_bytes", index)?)
            }
            "ws_raw_enabled" => {
                settings.ws_raw_enabled = Some(parse_bool(value).ok_or_else(|| {
                    format!("line {}: invalid ws_raw_enabled {:?}", index + 1, value)
                })?)
            }
            "ws_raw_max_bytes" => {
                settings.ws_raw_max_bytes = Some(parse_usize(value, "ws_raw_max_bytes", index)?)
            }
            "ws_raw_queue" => {
                settings.ws_raw_queue = Some(parse_usize(value, "ws_raw_queue", index)?)
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

fn parse_u64(value: &str, key: &str, index: usize) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("line {}: invalid {} {:?}", index + 1, key, value))
}

fn parse_usize(value: &str, key: &str, index: usize) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("line {}: invalid {} {:?}", index + 1, key, value))
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
        let directory = crate::test_support::temp_dir().join(format!("svcd-settings-{}", std::process::id()));
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
    fn parses_runtime_limit_and_log_keys() {
        let settings = parse(
            "logs_dir = logs\n\
             log_max_bytes = 65536\n\
             log_rotations = 2\n\
             log_to_file = off\n\
             request_stack_bytes = 131072\n\
             io_stack_bytes = 65536\n\
             blocking_threads = 4\n\
             max_connections = 32\n\
             max_connections_per_ip = 8\n\
             max_subscriptions = 4\n\
             max_subscribers_per_plugin = 4\n\
             http_idle_timeout_secs = 30\n\
             max_body_bytes = 8388608\n\
             max_json_body_bytes = 65536\n\
             max_complete_body_bytes = 131072\n\
             max_header_bytes = 8192\n\
             max_streams = 8\n\
             stream_buffer_bytes = 16384\n\
             max_package_bytes = 8388608\n\
             transfer_timeout_secs = 120\n\
             control_timeout_secs = 30\n\
             default_timeout = 45000\n\
             max_timeout = 450000\n",
            Path::new("/etc/svcd"),
        )
        .expect("parse");
        assert_eq!(settings.logs_dir, Some(PathBuf::from("/etc/svcd/logs")));
        assert_eq!(settings.log_max_bytes, Some(65536));
        assert_eq!(settings.log_rotations, Some(2));
        assert_eq!(settings.log_to_file, Some(false));
        assert_eq!(settings.request_stack_bytes, Some(131072));
        assert_eq!(settings.io_stack_bytes, Some(65536));
        assert_eq!(settings.blocking_threads, Some(4));
        assert_eq!(settings.max_connections, Some(32));
        assert_eq!(settings.max_connections_per_ip, Some(8));
        assert_eq!(settings.max_subscriptions, Some(4));
        assert_eq!(settings.max_subscribers_per_plugin, Some(4));
        assert_eq!(settings.http_idle_timeout_secs, Some(30));
        assert_eq!(settings.max_body_bytes, Some(8 * 1024 * 1024));
        assert_eq!(settings.max_json_body_bytes, Some(65536));
        assert_eq!(settings.max_complete_body_bytes, Some(131072));
        assert_eq!(settings.max_header_bytes, Some(8192));
        assert_eq!(settings.max_streams, Some(8));
        assert_eq!(settings.stream_buffer_bytes, Some(16384));
        assert_eq!(settings.max_package_bytes, Some(8 * 1024 * 1024));
        assert_eq!(settings.transfer_timeout_secs, Some(120));
        assert_eq!(settings.control_timeout_secs, Some(30));
        assert_eq!(settings.default_timeout_ms, Some(45_000));
        assert_eq!(settings.max_timeout_ms, Some(450_000));
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
        let directory = crate::test_support::temp_dir().join(format!(
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
