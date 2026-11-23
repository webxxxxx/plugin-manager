//! Runtime settings loaded from the `svcd.conf` chosen by `install`.
//!
//! The file is a small `key = value` document. The install layer resolves
//! which file to use (explicit install path, program directory, config
//! directory, or a generated default); this module parses and rewrites it but
//! keeps no process-wide copy — the composition root derives [`Config`] from
//! one [`load`] and passes the value down.
//!
//! [`Config`]: crate::platform::config::Config
//!
//! Every key can be overridden by `SVCD_<KEY>` (the key upper-cased), applied
//! once when the document is loaded. The environment wins over the file; an
//! empty variable means "not set".

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::platform::resources::Workload;

/// A removed path key that is still present in an old document.
///
/// The key is ignored; it is kept only so the bootstrap can explain which
/// path is actually used. See ADR-0023.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredPathKey {
    pub key: &'static str,
    pub value: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// HTTP listener address. Default: `127.0.0.1:8787`.
    pub bind: Option<SocketAddr>,
    /// Root for input: `svcd.conf`, `services.d`, `trust.d`, `compat.conf`.
    pub config_dir: Option<PathBuf>,
    /// Root for runtime state: `plugins`, `temp`, `run`.
    pub data_dir: Option<PathBuf>,
    pub auth_passwd: Option<String>,
    /// Removed path keys found in the document. They are never read as paths;
    /// the bootstrap warns about them and uses the derived value instead.
    pub ignored_path_keys: Vec<IgnoredPathKey>,
    /// Files under `data/temp` older than this are removed. Default: 2 hours.
    pub temp_retention_secs: Option<u64>,
    /// How often the supervisor scans `temp_dir`. `0` disables cleanup.
    pub temp_cleanup_interval_secs: Option<u64>,
    /// How often the supervisor compares the `services.d` fingerprint and
    /// reconciles. Milliseconds; `0` disables the periodic rescan. Default:
    /// 5000.
    pub rescan_interval_ms: Option<u64>,
    /// Removed on 2026-11-22 (ADR-0005 amendment): the daemon no longer waits
    /// for `services.d` at boot. The key is still parsed so an old document
    /// boots, but the bootstrap warns and ignores the value.
    pub boot_scan_wait_ms: Option<u64>,
    /// Per-service log directory. Empty/absent uses the platform or
    /// `install_dir/logs` default.
    pub logs_dir: Option<PathBuf>,
    /// Optional read-only static site root. When set, the daemon serves files
    /// under this directory at `/static/`. Relative values resolve against the
    /// directory containing `svcd.conf`, like the other paths.
    pub static_root: Option<PathBuf>,
    /// Rotation threshold for one log file. Default is chosen by the workload
    /// shape (it does not scale with the memory budget).
    pub log_max_bytes: Option<u64>,
    /// Number of rotated files to keep (1 = only the current file).
    pub log_rotations: Option<usize>,
    /// `false` keeps logs on stderr/syslog only. Default: true.
    pub log_to_file: Option<bool>,
    pub log_level: Option<crate::platform::log::Level>,
    pub log: Option<bool>,
    /// Explicit thread stack sizes. The workload shape supplies the defaults.
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
    /// Queued management requests between the server loop and the supervisor
    /// loop. Derived from the memory budget; clamped 8..4096.
    pub pending_queue: Option<usize>,
    /// Progress events queued between the reactor and one MCP stream. Derived
    /// from the memory budget; clamped 4..1024.
    pub progress_queue: Option<usize>,
    /// Whether an MCP client that accepts both JSON and SSE gets a stream.
    /// Default false: the one-JSON body is the cheaper path (no stream slot,
    /// no reactor hop), and a client that only accepts SSE still streams.
    pub mcp_streaming: Option<bool>,
    /// Jobs the blocking pool may queue before it answers 503 busy. Derived
    /// from the connection count (a queued job belongs to an admitted
    /// connection, so a larger queue can never be used); clamped to 1..1024.
    pub blocking_queue: Option<usize>,
    /// The share of this machine's CPU the daemon may use, 0..100. 0 (the
    /// default) means no cap. Above 0 the daemon installs the platform's hard
    /// cap when it can (a cgroup cpu.max on Linux) and always sheds new
    /// connections once its own CPU use stays above the target.
    pub cpu_budget_percent: Option<usize>,
    /// The peak memory this process may plan for, in bytes. `auto` (or absent)
    /// discounts the probed free memory; a number is taken as written and only
    /// clamped by the floor and the compile-time ceiling. See
    /// [`crate::platform::limits`].
    pub mem_budget_bytes: Option<u64>,
    /// The peak memory this process may plan for, as a percentage of the same
    /// probe `auto` uses (available memory). Mutually exclusive with
    /// `mem_budget_bytes`: the document asks for a size one way or the other.
    /// Resolved against the probe at startup and only clamped by the
    /// compile-time ceiling, like an explicit number.
    pub mem_budget_percent: Option<usize>,
    /// How the budget is spent. Absent derives the shape from the budget:
    /// below 512 MiB `tiny`, below 2 GiB `interactive`, otherwise `streaming`.
    pub workload: Option<Workload>,
    /// Probe inputs stated by the document. Each one is taken as written and
    /// the matching OS read is skipped, which is how a container with an
    /// unhelpful `/proc` — or a reproduction of someone else's machine — states
    /// its resources without an environment override.
    pub probe_available_memory: Option<u64>,
    /// What the memory cgroup still allows (`memory.max - memory.current`).
    pub probe_cgroup_remaining: Option<u64>,
    pub probe_cpus: Option<usize>,
    pub probe_install_free_disk: Option<u64>,
    pub probe_temp_free_disk: Option<u64>,
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
    /// The probe inputs this document states; every other item is probed.
    pub fn probe_overrides(&self) -> crate::platform::resources::ProbeOverrides {
        crate::platform::resources::ProbeOverrides {
            available_memory: self.probe_available_memory,
            cgroup_remaining: self.probe_cgroup_remaining,
            cpus: self.probe_cpus,
            install_free_disk: self.probe_install_free_disk,
            temp_free_disk: self.probe_temp_free_disk,
        }
    }
    pub fn temp_retention(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.temp_retention_secs.unwrap_or(2 * 60 * 60))
    }

    pub fn temp_cleanup_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.temp_cleanup_interval_secs.unwrap_or(5 * 60))
    }

    /// The supervisor's periodic rescan cadence. `0` means "never rescan".
    pub fn rescan_interval_ms(&self) -> u64 {
        self.rescan_interval_ms.unwrap_or(5_000)
    }
}

/// Every `svcd.conf` key that an environment variable can override.
///
/// The name is `SVCD_` + the key upper-cased (`max_streams` ->
/// `SVCD_MAX_STREAMS`). Keeping one list makes the override surface explicit:
/// an unknown `SVCD_*` variable (a test hook or a typo) is not silently
/// treated as a config key.
pub(crate) const ENV_KEYS: &[&str] = &[
    "bind",
    "allow_insecure_install",
    "config_dir",
    "data_dir",
    "logs_dir",
    "static_root",
    "install_dir",
    "auth_passwd",
    "temp_retention_secs",
    "temp_cleanup_interval_secs",
    "rescan_interval_ms",
    // Removed from behavior (ADR-0005 amendment); kept so an existing
    // environment cannot turn an unknown key into a startup error.
    "boot_scan_wait_ms",
    "log_max_bytes",
    "log_rotations",
    "log_to_file",
    "log_level",
    "log",
    "request_stack_bytes",
    "io_stack_bytes",
    "blocking_threads",
    "blocking_queue",
    "mcp_streaming",
    "max_body_bytes",
    "max_json_body_bytes",
    "max_complete_body_bytes",
    "max_header_bytes",
    "max_streams",
    "stream_buffer_bytes",
    "max_connections",
    "max_connections_per_ip",
    "max_subscriptions",
    "max_subscribers_per_plugin",
    "http_idle_timeout_secs",
    "max_package_bytes",
    "transfer_timeout_secs",
    "control_timeout_secs",
    "default_timeout",
    "max_timeout",
    "ws_max_message_bytes",
    "ws_inflight_bytes",
    "ws_inflight_requests",
    "ws_buffer_shrink_bytes",
    "ws_raw_enabled",
    "ws_raw_max_bytes",
    "ws_raw_queue",
    "pending_queue",
    "progress_queue",
    "cpu_budget_percent",
    "mem_budget_bytes",
    "mem_budget_percent",
    "workload",
    "probe_available_memory",
    "probe_cgroup_remaining",
    "probe_cpus",
    "probe_install_free_disk",
    "probe_temp_free_disk",
];

/// Build a document from the environment and append it after the file, so the
/// normal parser sees the override last and every validation rule stays in one
/// place. Empty values mean "not set"; a multi-line value cannot be a config
/// line and is rejected instead of being silently truncated.
fn environment_document(env: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut text = String::new();
    for key in ENV_KEYS {
        let name = format!("SVCD_{}", key.to_ascii_uppercase());
        let Some(value) = env(&name) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        if value.contains(['\n', '\r']) {
            return Err(format!("{} must not contain a newline", name));
        }
        text.push_str(key);
        text.push_str(" = ");
        text.push_str(&value);
        text.push('\n');
    }
    Ok(text)
}

/// Load the document this process runs with.
///
/// One call resolves the file *and* parses it, so the composition root
/// ([`crate::platform::config::Config::bootstrap`]) gets both values from a single read.
/// Precedence: an explicit `--conf` (which must exist), then
/// `SVCD_CONFIG_DIR`, then `<program dir>/config/svcd.conf`, then
/// `<program dir>/svcd.conf`, then the platform config directory. A
/// missing file in a discovered location is an empty document, not an error; a
/// broken one is an error. There is no process cache: every caller decides once
/// and passes the value down.
pub(crate) fn load(conf: Option<&Path>) -> Result<(Settings, Option<PathBuf>), String> {
    if let Some(path) = conf {
        if !path.is_file() {
            return Err(format!("config not found: {}", path.display()));
        }
        return Ok((load_from(path)?, Some(path.to_path_buf())));
    }
    if let Some(dir) = std::env::var_os("SVCD_CONFIG_DIR") {
        let path = PathBuf::from(dir).join("svcd.conf");
        return Ok((load_from(&path)?, Some(path)));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        for path in [dir.join("config").join("svcd.conf"), dir.join("svcd.conf")] {
            if path.is_file() {
                return Ok((load_from(&path)?, Some(path)));
            }
        }
    }
    let path = crate::platform::host::dirs::default_config_dir().join("svcd.conf");
    Ok((load_from(&path)?, Some(path)))
}

fn load_from(path: &Path) -> Result<Settings, String> {
    let mut text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("read {}: {}", path.display(), error)),
    };
    let env = environment_document(|name| std::env::var(name).ok())?;
    if !env.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&env);
    }
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
        let value = strip_comment(value.trim());
        if key.is_empty() {
            return Err(format!("line {}: empty key", index + 1));
        }
        if value.is_empty() {
            return Err(format!("line {}: empty value for {}", index + 1, key));
        }
        // When adding or removing a key here, update ENV_KEYS above; that list
        // is the whole environment-override surface.
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
            "data_dir" => settings.data_dir = Some(resolve(base, value)),
            "static_root" => settings.static_root = Some(resolve(base, value)),
            "install_dir" => settings.install_dir = Some(resolve(base, value)),
            "auth_passwd" => settings.auth_passwd = Some(value.to_string()),
            // Removed in ADR-0023. Parsing them lets an old document boot; the
            // resolver ignores the value and the bootstrap warns with the
            // derived path. A syntax error in the line is still an error.
            "plugins_dir" | "services_dir" | "temp_dir" => {
                settings.ignored_path_keys.push(IgnoredPathKey {
                    key: match key {
                        "plugins_dir" => "plugins_dir",
                        "services_dir" => "services_dir",
                        _ => "temp_dir",
                    },
                    value: resolve(base, value),
                })
            }
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
            "rescan_interval_ms" => {
                settings.rescan_interval_ms = Some(parse_u64(value, "rescan_interval_ms", index)?)
            }
            "boot_scan_wait_ms" => {
                // Removed from behavior (ADR-0005 amendment); parsing it keeps
                // an old document bootable. The bootstrap warns and ignores it.
                settings.boot_scan_wait_ms = Some(parse_u64(value, "boot_scan_wait_ms", index)?)
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
            "log_level" => {
                settings.log_level =
                    Some(crate::platform::log::Level::parse(value).ok_or_else(|| {
                        let names: Vec<&str> = crate::platform::log::Level::ALL
                            .iter()
                            .map(|level| level.as_str().trim())
                            .collect();
                        format!(
                            "line {}: invalid log_level {:?} ({})",
                            index + 1,
                            value,
                            names.join("|")
                        )
                    })?)
            }
            "log" => {
                settings.log = Some(parse_bool(value).ok_or_else(|| {
                    format!("line {}: log must be on or off, got {:?}", index + 1, value)
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
            "mcp_streaming" => {
                settings.mcp_streaming =
                    Some(parse_bool(value).ok_or_else(|| {
                        format!("line {}: mcp_streaming must be on or off", index)
                    })?)
            }
            "blocking_queue" => {
                settings.blocking_queue = Some(parse_usize(value, "blocking_queue", index)?)
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
                settings.default_timeout_ms = Some(parse_u64(value, "default_timeout", index)?)
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
            "pending_queue" => {
                settings.pending_queue = Some(parse_usize(value, "pending_queue", index)?)
            }
            "cpu_budget_percent" => {
                settings.cpu_budget_percent = Some(parse_usize(value, "cpu_budget_percent", index)?)
            }
            "progress_queue" => {
                settings.progress_queue = Some(parse_usize(value, "progress_queue", index)?)
            }
            "mem_budget_bytes" => {
                settings.mem_budget_bytes = match value {
                    "auto" => None,
                    _ => Some(parse_u64(value, "mem_budget_bytes", index)?),
                }
            }
            "mem_budget_percent" => {
                let percent = parse_usize(value, "mem_budget_percent", index)?;
                if !(1..=100).contains(&percent) {
                    return Err(format!(
                        "line {}: mem_budget_percent must be 1..=100, got {percent}",
                        index + 1
                    ));
                }
                settings.mem_budget_percent = Some(percent);
            }
            "probe_available_memory" => {
                settings.probe_available_memory =
                    Some(parse_u64(value, "probe_available_memory", index)?)
            }
            "probe_cgroup_remaining" => {
                settings.probe_cgroup_remaining =
                    Some(parse_u64(value, "probe_cgroup_remaining", index)?)
            }
            "probe_cpus" => settings.probe_cpus = Some(parse_usize(value, "probe_cpus", index)?),
            "probe_install_free_disk" => {
                settings.probe_install_free_disk =
                    Some(parse_u64(value, "probe_install_free_disk", index)?)
            }
            "probe_temp_free_disk" => {
                settings.probe_temp_free_disk =
                    Some(parse_u64(value, "probe_temp_free_disk", index)?)
            }
            "workload" => {
                settings.workload = Some(Workload::parse(value).ok_or_else(|| {
                    format!(
                        "line {}: invalid workload {:?} ({})",
                        index + 1,
                        value,
                        Workload::ALL.map(Workload::as_str).join("|")
                    )
                })?)
            }
            other => return Err(format!("line {}: unknown key {:?}", index + 1, other)),
        }
    }
    // One size, stated one way. Both keys in one document is ambiguous — the
    // percentage would silently lose to the number, or the other way round.
    if settings.mem_budget_bytes.is_some() && settings.mem_budget_percent.is_some() {
        return Err(
            "mem_budget_bytes and mem_budget_percent are mutually exclusive; keep one".to_string(),
        );
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

/// A trailing `# ...` comment is allowed after a value when the hash is
/// preceded by whitespace. `svc config tune` writes the derivation on the same
/// line, and a hand-edited file may do the same.
fn strip_comment(value: &str) -> &str {
    match value.find(" #") {
        Some(index) => value[..index].trim_end(),
        None => value,
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
/// Used by `svc lan passwd` to write `auth_passwd` without a restart.
pub fn update_value(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {}", path.display(), error))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
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
    fn environment_document_uses_svcd_prefixed_keys() {
        let doc = environment_document(|name| match name {
            "SVCD_BIND" => Some("127.0.0.1:9".into()),
            "SVCD_MAX_STREAMS" => Some("7".into()),
            "SVCD_DATA_DIR" => Some("/env/data".into()),
            "SVCD_ALLOW_INSECURE_INSTALL" => Some("on".into()),
            _ => None,
        })
        .expect("env document");
        let settings = parse(&doc, Path::new("/etc/svcd")).expect("parse env");
        assert_eq!(
            settings.bind,
            Some("127.0.0.1:9".parse::<SocketAddr>().expect("bind"))
        );
        assert_eq!(settings.max_streams, Some(7));
        assert_eq!(settings.data_dir, Some(PathBuf::from("/env/data")));
        assert!(settings.allow_insecure_install);
    }

    #[test]
    fn environment_document_wins_over_the_file() {
        let file = parse("bind = 127.0.0.1:1\nmax_streams = 1\n", Path::new("/etc")).expect("file");
        let env = environment_document(|name| match name {
            "SVCD_BIND" => Some("127.0.0.1:2".into()),
            "SVCD_MAX_STREAMS" => Some("3".into()),
            _ => None,
        })
        .expect("env");
        let combined = format!("bind = 127.0.0.1:1\nmax_streams = 1\n{}", env);
        let settings = parse(&combined, Path::new("/etc")).expect("combined");
        assert_eq!(
            settings.bind,
            Some("127.0.0.1:2".parse::<SocketAddr>().expect("bind"))
        );
        assert_eq!(settings.max_streams, Some(3));
        assert_eq!(file.max_streams, Some(1));
    }

    #[test]
    fn environment_override_rejects_a_multiline_value() {
        let error = environment_document(|name| {
            (name == "SVCD_AUTH_PASSWD").then(|| "one\ntwo".to_string())
        })
        .expect_err("newline");
        assert!(error.contains("SVCD_AUTH_PASSWD"), "{error}");
    }

    #[test]
    fn an_empty_environment_value_is_not_an_override() {
        let doc =
            environment_document(|name| (name == "SVCD_BIND").then(String::new)).expect("empty");
        assert!(doc.is_empty(), "{doc}");
    }

    #[test]
    fn parses_all_keys_and_resolves_relative_paths() {
        let settings = parse(
            "\u{feff}# svcd runtime\nbind = 0.0.0.0:8787\nallow_insecure_install = on\nconfig_dir = /srv/svcd\ndata_dir = state\nlogs_dir = logs\nstatic_root = public\ninstall_dir = /etc/sr/svcd\nauth_passwd = pbkdf2-sha256$10000$c2FsdA$aGFzaA\nplugins_dir = plugins\nservices_dir = /var/lib/svcd/services.d\ntemp_dir = /var/tmp/svcd\ntemp_retention_secs = 3600\ntemp_cleanup_interval_secs = 60\nrescan_interval_ms = 250\nboot_scan_wait_ms = 0\n",
            Path::new("/etc/svcd"),
        )
        .expect("parse");
        assert_eq!(
            settings.bind,
            Some("0.0.0.0:8787".parse::<SocketAddr>().expect("bind"))
        );
        assert!(settings.allow_insecure_install);
        assert_eq!(settings.config_dir, Some(PathBuf::from("/srv/svcd")));
        assert_eq!(settings.data_dir, Some(PathBuf::from("/etc/svcd/state")));
        assert_eq!(settings.logs_dir, Some(PathBuf::from("/etc/svcd/logs")));
        assert_eq!(
            settings.static_root,
            Some(PathBuf::from("/etc/svcd/public"))
        );
        assert_eq!(settings.install_dir, Some(PathBuf::from("/etc/sr/svcd")));
        assert_eq!(
            settings.auth_passwd.as_deref(),
            Some("pbkdf2-sha256$10000$c2FsdA$aGFzaA")
        );
        assert_eq!(
            settings
                .ignored_path_keys
                .iter()
                .map(|ignored| ignored.key)
                .collect::<Vec<_>>(),
            vec!["plugins_dir", "services_dir", "temp_dir"]
        );
        assert_eq!(settings.temp_retention_secs, Some(3600));
        assert_eq!(settings.temp_cleanup_interval_secs, Some(60));
        assert_eq!(settings.rescan_interval_ms, Some(250));
        // Removed from behavior but still accepted so an old document boots.
        assert_eq!(settings.boot_scan_wait_ms, Some(0));
        assert_eq!(settings.rescan_interval_ms(), 250);
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
        let directory = crate::platform::test_support::temp_dir()
            .join(format!("svcd-settings-{}", std::process::id()));
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
             log_level = warn\n\
             log = off\n\
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
        assert_eq!(settings.log_level, Some(crate::platform::log::Level::Warn));
        assert_eq!(settings.log, Some(false));
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
    fn the_rescan_cadence_defaults_to_five_seconds() {
        let settings = Settings::default();
        assert_eq!(settings.rescan_interval_ms(), 5_000);
        // The boot wait was removed (ADR-0005 amendment): no default exists.
        assert_eq!(settings.boot_scan_wait_ms, None);
    }

    /// Without an env override the document value survives the bootstrap pass;
    /// the override path itself is covered end to end by the tests that start a
    /// daemon with `SVCD_RESCAN_INTERVAL_MS`.
    #[test]
    fn relative_install_dir_resolves_against_the_conf_directory() {
        let settings = parse("install_dir = sr/svcd\n", Path::new("/etc/svcd")).expect("parse");
        assert_eq!(
            settings.install_dir,
            Some(PathBuf::from("/etc/svcd/sr/svcd"))
        );
    }

    #[test]
    fn update_value_replaces_or_appends_one_key() {
        let directory = crate::platform::test_support::temp_dir().join(format!(
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
            "log_level = loud\n",
            "log = maybe\n",
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
            parse("data_dir = C:\\svcd\\data\n", Path::new("C:\\config")).expect("parse");
        assert_eq!(settings.data_dir, Some(PathBuf::from(r"C:\svcd\data")));
    }
}

/// Replace or append one delimited block of a `svcd.conf` document.
///
/// Everything outside `begin`..`end` is preserved byte for byte, so the block
/// `svc config tune --write` owns can be re-written on every run and removed by
/// deleting its two marker lines. An unterminated block is an error: rewriting a
/// document whose structure cannot be trusted would lose the operator's edit.
pub fn upsert_block(path: &Path, begin: &str, end: &str, body: &str) -> Result<(), String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        // A host that never had a `svcd.conf` gets the block as its document.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("read {}: {}", path.display(), error)),
    };
    let mut output = String::with_capacity(text.len() + body.len() + 128);
    let mut inside = false;
    let mut wrote = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if inside {
            if trimmed == end {
                inside = false;
            }
            continue;
        }
        if trimmed == begin {
            output.push_str(&block(begin, end, body));
            wrote = true;
            inside = true;
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }
    if inside {
        return Err(format!(
            "{}: the {:?} block has no {:?} line",
            path.display(),
            begin,
            end
        ));
    }
    if !wrote {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&block(begin, end, body));
    }
    fs::write(path, output).map_err(|error| format!("write {}: {}", path.display(), error))
}

fn block(begin: &str, end: &str, body: &str) -> String {
    let mut out = String::with_capacity(body.len() + begin.len() + end.len() + 4);
    out.push_str(begin);
    out.push('\n');
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(end);
    out.push('\n');
    out
}

#[cfg(test)]
mod document_tests;
