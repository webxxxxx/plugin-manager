//! Compile-time memory profile plus runtime overrides.
//!
//! `MEM_LEVEL` (see `build.rs`) picks the defaults and the hard ceilings for
//! buffers and thread stacks. `svcd.conf` may lower them per host; values are
//! clamped to a safe range so a typo cannot turn a 50 MB router into a swap
//! storm.

use std::time::Duration;

use crate::platform::settings;

#[cfg(mem_low)]
mod defaults {
    pub const MAX_BODY: u64 = 8 * 1024 * 1024;
    pub const MAX_JSON_BODY: u64 = 64 * 1024;
    pub const MAX_COMPLETE_BODY: u64 = 128 * 1024;
    pub const MAX_HEADER: usize = 8 * 1024;
    pub const MAX_STREAMS: usize = 8;
    pub const STREAM_BUFFER: usize = 16 * 1024;
    pub const MAX_REQUEST_STACK: usize = 128 * 1024;
    pub const MAX_IO_STACK: usize = 64 * 1024;
    pub const MAX_CONNECTIONS: usize = 32;
    pub const MAX_CONNECTIONS_PER_IP: usize = 8;
    pub const MAX_SUBSCRIPTIONS: usize = 4;
    pub const MAX_SUBSCRIBERS_PER_PLUGIN: usize = 4;
    pub const IDLE_TIMEOUT_SECS: u64 = 30;
    pub const LOG_MAX_BYTES: u64 = 64 * 1024;
    pub const LOG_ROTATIONS: usize = 3;
    pub const MAX_PACKAGE_BYTES: u64 = 8 * 1024 * 1024;
    pub const WS_MAX_MESSAGE: usize = 1024 * 1024;
    pub const WS_INFLIGHT_BYTES: usize = 2 * 1024 * 1024;
    pub const WS_INFLIGHT_REQUESTS: usize = 4;
    pub const WS_BUFFER_SHRINK: usize = 64 * 1024;
    pub const WS_RAW_MAX: usize = 1024 * 1024;
}

#[cfg(mem_middle)]
mod defaults {
    pub const MAX_BODY: u64 = 64 * 1024 * 1024;
    pub const MAX_JSON_BODY: u64 = 256 * 1024;
    pub const MAX_COMPLETE_BODY: u64 = 1024 * 1024;
    pub const MAX_HEADER: usize = 16 * 1024;
    pub const MAX_STREAMS: usize = 16;
    pub const STREAM_BUFFER: usize = 32 * 1024;
    pub const MAX_REQUEST_STACK: usize = 192 * 1024;
    pub const MAX_IO_STACK: usize = 96 * 1024;
    pub const MAX_CONNECTIONS: usize = 128;
    pub const MAX_CONNECTIONS_PER_IP: usize = 32;
    pub const MAX_SUBSCRIPTIONS: usize = 16;
    pub const MAX_SUBSCRIBERS_PER_PLUGIN: usize = 16;
    pub const IDLE_TIMEOUT_SECS: u64 = 60;
    pub const LOG_MAX_BYTES: u64 = 64 * 1024;
    pub const LOG_ROTATIONS: usize = 3;
    pub const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
    pub const WS_MAX_MESSAGE: usize = 1024 * 1024;
    pub const WS_INFLIGHT_BYTES: usize = 8 * 1024 * 1024;
    pub const WS_INFLIGHT_REQUESTS: usize = 16;
    pub const WS_BUFFER_SHRINK: usize = 64 * 1024;
    pub const WS_RAW_MAX: usize = 1024 * 1024;
}

#[cfg(mem_high)]
mod defaults {
    pub const MAX_BODY: u64 = 512 * 1024 * 1024;
    pub const MAX_JSON_BODY: u64 = 1024 * 1024;
    pub const MAX_COMPLETE_BODY: u64 = 8 * 1024 * 1024;
    pub const MAX_HEADER: usize = 64 * 1024;
    pub const MAX_STREAMS: usize = 64;
    pub const STREAM_BUFFER: usize = 64 * 1024;
    pub const MAX_REQUEST_STACK: usize = 256 * 1024;
    pub const MAX_IO_STACK: usize = 128 * 1024;
    pub const MAX_CONNECTIONS: usize = 512;
    pub const MAX_CONNECTIONS_PER_IP: usize = 64;
    pub const MAX_SUBSCRIPTIONS: usize = 64;
    pub const MAX_SUBSCRIBERS_PER_PLUGIN: usize = 64;
    pub const IDLE_TIMEOUT_SECS: u64 = 120;
    pub const LOG_MAX_BYTES: u64 = 64 * 1024;
    pub const LOG_ROTATIONS: usize = 3;
    pub const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
    pub const WS_MAX_MESSAGE: usize = 1024 * 1024;
    pub const WS_INFLIGHT_BYTES: usize = 64 * 1024 * 1024;
    pub const WS_INFLIGHT_REQUESTS: usize = 64;
    pub const WS_BUFFER_SHRINK: usize = 64 * 1024;
    pub const WS_RAW_MAX: usize = 1024 * 1024;
}

pub use defaults::{
    MAX_BODY, MAX_COMPLETE_BODY, MAX_CONNECTIONS, MAX_CONNECTIONS_PER_IP, MAX_HEADER, MAX_IO_STACK,
    MAX_JSON_BODY, MAX_PACKAGE_BYTES, MAX_REQUEST_STACK, MAX_STREAMS, MAX_SUBSCRIBERS_PER_PLUGIN,
    MAX_SUBSCRIPTIONS, STREAM_BUFFER,
};

pub const WS_RAW_QUEUE: usize = 128;

/// Default blocking-pool workers. The pool runs face handlers that cannot run
/// on the server loop (control-channel round trips, disk work).
pub const DEFAULT_BLOCKING_THREADS: usize = 2;
/// A pool smaller than two workers cannot both run a slow job and keep a second
/// one moving; the spec fixes the floor at two.
pub const MIN_BLOCKING_THREADS: usize = 2;
/// Upper bound so a typo cannot turn a small host into a thread farm.
pub const MAX_BLOCKING_THREADS: usize = 8;
/// Runtime view of the limits. `svcd.conf` may lower (or, within a sane range,
/// raise) the compile-time defaults.
#[derive(Debug, Clone)]
pub struct Limits {
    pub request_stack_bytes: usize,
    pub io_stack_bytes: usize,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub blocking_threads: usize,
    pub max_body: u64,
    pub max_json_body: u64,
    pub max_complete_body: u64,
    pub max_header: usize,
    pub max_streams: usize,
    pub stream_buffer: usize,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub max_connections: usize,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub max_connections_per_ip: usize,
    pub max_subscriptions: usize,
    pub max_subscribers_per_plugin: usize,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub idle_timeout: Duration,
    pub log_max_bytes: u64,
    pub log_rotations: usize,
    pub log_to_file: bool,
    pub max_package_bytes: u64,
    pub transfer_timeout: Duration,
    /// Bound for one management route's control-channel round trip. The
    /// server stamps a blocking job with this deadline; an unresponsive
    /// supervisor then answers 504 instead of pinning a worker and a
    /// connection. Clamped 1..300 s; a daemon restart applies a change.
    pub control_timeout: Duration,
    /// 默认调用超时；契约的 `default_timeout` 从这里来。
    pub default_timeout: Duration,
    /// 调用超时的上限；契约的 `max_timeout` 从这里来。
    pub max_timeout: Duration,
    pub ws_max_message_bytes: usize,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub ws_inflight_bytes: usize,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub ws_inflight_requests: usize,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub ws_buffer_shrink_bytes: usize,
    pub ws_raw_enabled: bool,
    pub ws_raw_max_bytes: usize,
    pub ws_raw_queue: usize,
}

impl Limits {
    fn from_settings(settings: SettingsView<'_>) -> Limits {
        Limits {
            request_stack_bytes: clamp_usize(
                settings.request_stack_bytes,
                MAX_REQUEST_STACK,
                64 * 1024,
                8 * 1024 * 1024,
            ),
            io_stack_bytes: clamp_usize(
                settings.io_stack_bytes,
                MAX_IO_STACK,
                32 * 1024,
                4 * 1024 * 1024,
            ),
            blocking_threads: clamp_usize(
                settings.blocking_threads,
                DEFAULT_BLOCKING_THREADS,
                MIN_BLOCKING_THREADS,
                MAX_BLOCKING_THREADS,
            ),
            max_body: clamp_u64(
                settings.max_body_bytes,
                MAX_BODY,
                64 * 1024,
                1024 * 1024 * 1024,
            ),
            max_json_body: clamp_u64(
                settings.max_json_body_bytes,
                MAX_JSON_BODY,
                4 * 1024,
                64 * 1024 * 1024,
            ),
            max_complete_body: clamp_u64(
                settings.max_complete_body_bytes,
                MAX_COMPLETE_BODY,
                16 * 1024,
                128 * 1024 * 1024,
            ),
            max_header: clamp_usize(settings.max_header_bytes, MAX_HEADER, 1024, 1024 * 1024),
            max_streams: clamp_usize(settings.max_streams, MAX_STREAMS, 1, 1024),
            stream_buffer: clamp_usize(
                settings.stream_buffer_bytes,
                STREAM_BUFFER,
                4 * 1024,
                1024 * 1024,
            ),
            max_connections: clamp_usize(settings.max_connections, MAX_CONNECTIONS, 1, 4096),
            max_connections_per_ip: clamp_usize(
                settings.max_connections_per_ip,
                MAX_CONNECTIONS_PER_IP,
                1,
                1024,
            ),
            max_subscriptions: clamp_usize(settings.max_subscriptions, MAX_SUBSCRIPTIONS, 0, 4096),
            max_subscribers_per_plugin: clamp_usize(
                settings.max_subscribers_per_plugin,
                MAX_SUBSCRIBERS_PER_PLUGIN,
                0,
                1024,
            ),
            idle_timeout: Duration::from_secs(
                settings
                    .idle_timeout_secs
                    .unwrap_or(defaults::IDLE_TIMEOUT_SECS)
                    .clamp(1, 3600),
            ),
            log_max_bytes: clamp_u64(
                settings.log_max_bytes,
                defaults::LOG_MAX_BYTES,
                4 * 1024,
                16 * 1024 * 1024,
            ),
            log_rotations: clamp_usize(settings.log_rotations, defaults::LOG_ROTATIONS, 1, 10),
            log_to_file: settings.log_to_file.unwrap_or(true),
            max_package_bytes: clamp_u64(
                settings.max_package_bytes,
                MAX_PACKAGE_BYTES,
                1024 * 1024,
                1024 * 1024 * 1024,
            ),
            transfer_timeout: Duration::from_secs(
                settings
                    .transfer_timeout_secs
                    .unwrap_or(120)
                    .clamp(10, 3600),
            ),
            control_timeout: Duration::from_secs(
                settings.control_timeout_secs.unwrap_or(30).clamp(1, 300),
            ),
            default_timeout: Duration::from_millis(
                settings
                    .default_timeout_ms
                    .unwrap_or(30_000)
                    .clamp(100, 300_000),
            ),
            max_timeout: {
                let default = settings
                    .default_timeout_ms
                    .unwrap_or(30_000)
                    .clamp(100, 300_000);
                Duration::from_millis(
                    settings
                        .max_timeout_ms
                        .unwrap_or(300_000)
                        .clamp(default, 600_000),
                )
            },
            ws_max_message_bytes: clamp_usize(
                settings.ws_max_message_bytes,
                defaults::WS_MAX_MESSAGE,
                1024,
                16 * 1024 * 1024,
            ),
            ws_inflight_bytes: clamp_usize(
                settings.ws_inflight_bytes,
                defaults::WS_INFLIGHT_BYTES,
                64 * 1024,
                1024 * 1024 * 1024,
            ),
            ws_inflight_requests: clamp_usize(
                settings.ws_inflight_requests,
                defaults::WS_INFLIGHT_REQUESTS,
                1,
                4096,
            ),
            ws_buffer_shrink_bytes: clamp_usize(
                settings.ws_buffer_shrink_bytes,
                defaults::WS_BUFFER_SHRINK,
                4 * 1024,
                16 * 1024 * 1024,
            ),
            ws_raw_enabled: settings.ws_raw_enabled.unwrap_or(true),
            ws_raw_max_bytes: clamp_usize(
                settings.ws_raw_max_bytes,
                defaults::WS_RAW_MAX,
                1024,
                16 * 1024 * 1024,
            ),
            ws_raw_queue: clamp_usize(settings.ws_raw_queue, WS_RAW_QUEUE, 4, 4096),
        }
    }
}

struct SettingsView<'a> {
    request_stack_bytes: Option<usize>,
    io_stack_bytes: Option<usize>,
    blocking_threads: Option<usize>,
    max_body_bytes: Option<u64>,
    max_json_body_bytes: Option<u64>,
    max_complete_body_bytes: Option<u64>,
    max_header_bytes: Option<usize>,
    max_streams: Option<usize>,
    stream_buffer_bytes: Option<usize>,
    max_connections: Option<usize>,
    max_connections_per_ip: Option<usize>,
    max_subscriptions: Option<usize>,
    max_subscribers_per_plugin: Option<usize>,
    idle_timeout_secs: Option<u64>,
    log_max_bytes: Option<u64>,
    log_rotations: Option<usize>,
    log_to_file: Option<bool>,
    max_package_bytes: Option<u64>,
    transfer_timeout_secs: Option<u64>,
    control_timeout_secs: Option<u64>,
    default_timeout_ms: Option<u64>,
    max_timeout_ms: Option<u64>,
    ws_max_message_bytes: Option<usize>,
    ws_inflight_bytes: Option<usize>,
    ws_inflight_requests: Option<usize>,
    ws_buffer_shrink_bytes: Option<usize>,
    ws_raw_enabled: Option<bool>,
    ws_raw_max_bytes: Option<usize>,
    ws_raw_queue: Option<usize>,
    #[allow(dead_code)]
    marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> SettingsView<'a> {
    fn from(settings: &'a settings::Settings) -> Self {
        SettingsView {
            request_stack_bytes: settings.request_stack_bytes,
            io_stack_bytes: settings.io_stack_bytes,
            blocking_threads: settings.blocking_threads,
            max_body_bytes: settings.max_body_bytes,
            max_json_body_bytes: settings.max_json_body_bytes,
            max_complete_body_bytes: settings.max_complete_body_bytes,
            max_header_bytes: settings.max_header_bytes,
            max_streams: settings.max_streams,
            stream_buffer_bytes: settings.stream_buffer_bytes,
            max_connections: settings.max_connections,
            max_connections_per_ip: settings.max_connections_per_ip,
            max_subscriptions: settings.max_subscriptions,
            max_subscribers_per_plugin: settings.max_subscribers_per_plugin,
            idle_timeout_secs: settings.http_idle_timeout_secs,
            log_max_bytes: settings.log_max_bytes,
            log_rotations: settings.log_rotations,
            log_to_file: settings.log_to_file,
            max_package_bytes: settings.max_package_bytes,
            transfer_timeout_secs: settings.transfer_timeout_secs,
            control_timeout_secs: settings.control_timeout_secs,
            default_timeout_ms: settings.default_timeout_ms,
            max_timeout_ms: settings.max_timeout_ms,
            ws_max_message_bytes: settings.ws_max_message_bytes,
            ws_inflight_bytes: settings.ws_inflight_bytes,
            ws_inflight_requests: settings.ws_inflight_requests,
            ws_buffer_shrink_bytes: settings.ws_buffer_shrink_bytes,
            ws_raw_enabled: settings.ws_raw_enabled,
            ws_raw_max_bytes: settings.ws_raw_max_bytes,
            ws_raw_queue: settings.ws_raw_queue,
            marker: std::marker::PhantomData,
        }
    }
}

fn clamp_usize(value: Option<usize>, default: usize, min: usize, max: usize) -> usize {
    value.unwrap_or(default).clamp(min, max)
}

fn clamp_u64(value: Option<u64>, default: u64, min: u64, max: u64) -> u64 {
    value.unwrap_or(default).clamp(min, max)
}

/// Derive the limits from a parsed `svcd.conf`.
///
/// Pure: it reads only the document, so the composition root can derive the
/// limits once and a test can derive them from its own settings without
/// touching process state.
pub fn derive(settings: &settings::Settings) -> Limits {
    Limits::from_settings(SettingsView::from(settings))
}

/// The limits an unconfigured process would run with. Unit tests use this
/// instead of re-deriving from `Settings::default()` at every seam.
#[cfg(test)]
pub(crate) fn test_default() -> Limits {
    derive(&settings::Settings::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits_with(settings: &settings::Settings) -> Limits {
        Limits::from_settings(SettingsView::from(settings))
    }

    #[test]
    fn idle_timeout_is_clamped() {
        let mut settings = settings::Settings::default();
        settings.http_idle_timeout_secs = Some(0);
        let limits = limits_with(&settings);
        assert_eq!(limits.idle_timeout, Duration::from_secs(1));

        settings.http_idle_timeout_secs = Some(u64::MAX);
        let limits = limits_with(&settings);
        assert_eq!(limits.idle_timeout, Duration::from_secs(3600));
    }

    #[test]
    fn control_timeout_defaults_to_thirty_seconds() {
        let limits = limits_with(&settings::Settings::default());
        assert_eq!(limits.control_timeout, Duration::from_secs(30));
    }

    #[test]
    fn every_limit_has_a_floor() {
        let mut settings = settings::Settings::default();
        settings.request_stack_bytes = Some(0);
        settings.io_stack_bytes = Some(0);
        settings.blocking_threads = Some(0);
        settings.max_body_bytes = Some(0);
        settings.max_json_body_bytes = Some(0);
        settings.max_complete_body_bytes = Some(0);
        settings.max_header_bytes = Some(0);
        settings.max_streams = Some(0);
        settings.stream_buffer_bytes = Some(0);
        settings.max_connections = Some(0);
        settings.max_connections_per_ip = Some(0);
        settings.max_subscriptions = Some(0);
        settings.max_subscribers_per_plugin = Some(0);
        settings.http_idle_timeout_secs = Some(0);
        settings.log_max_bytes = Some(0);
        settings.log_rotations = Some(0);
        settings.max_package_bytes = Some(0);
        settings.transfer_timeout_secs = Some(0);
        settings.control_timeout_secs = Some(0);
        settings.default_timeout_ms = Some(0);
        settings.max_timeout_ms = Some(0);
        settings.ws_max_message_bytes = Some(0);
        settings.ws_inflight_bytes = Some(0);
        settings.ws_inflight_requests = Some(0);
        settings.ws_buffer_shrink_bytes = Some(0);
        settings.ws_raw_max_bytes = Some(0);
        settings.ws_raw_queue = Some(0);

        let limits = limits_with(&settings);
        assert_eq!(limits.request_stack_bytes, 64 * 1024);
        assert_eq!(limits.io_stack_bytes, 32 * 1024);
        assert_eq!(limits.blocking_threads, 2);
        assert_eq!(limits.max_body, 64 * 1024);
        assert_eq!(limits.max_json_body, 4 * 1024);
        assert_eq!(limits.max_complete_body, 16 * 1024);
        assert_eq!(limits.max_header, 1024);
        assert_eq!(limits.max_streams, 1);
        assert_eq!(limits.stream_buffer, 4 * 1024);
        assert_eq!(limits.max_connections, 1);
        assert_eq!(limits.max_connections_per_ip, 1);
        assert_eq!(limits.max_subscriptions, 0);
        assert_eq!(limits.max_subscribers_per_plugin, 0);
        assert_eq!(limits.idle_timeout, Duration::from_secs(1));
        assert_eq!(limits.log_max_bytes, 4 * 1024);
        assert_eq!(limits.log_rotations, 1);
        assert_eq!(limits.max_package_bytes, 1024 * 1024);
        assert_eq!(limits.transfer_timeout, Duration::from_secs(10));
        assert_eq!(limits.control_timeout, Duration::from_secs(1));
        assert_eq!(limits.default_timeout, Duration::from_millis(100));
        assert_eq!(limits.max_timeout, Duration::from_millis(100));
        assert_eq!(limits.ws_max_message_bytes, 1024);
        assert_eq!(limits.ws_inflight_bytes, 64 * 1024);
        assert_eq!(limits.ws_inflight_requests, 1);
        assert_eq!(limits.ws_buffer_shrink_bytes, 4 * 1024);
        assert_eq!(limits.ws_raw_max_bytes, 1024);
        assert_eq!(limits.ws_raw_queue, 4);
    }

    #[test]
    fn every_limit_has_a_ceiling() {
        let mut settings = settings::Settings::default();
        settings.request_stack_bytes = Some(usize::MAX);
        settings.io_stack_bytes = Some(usize::MAX);
        settings.blocking_threads = Some(usize::MAX);
        settings.max_body_bytes = Some(u64::MAX);
        settings.max_json_body_bytes = Some(u64::MAX);
        settings.max_complete_body_bytes = Some(u64::MAX);
        settings.max_header_bytes = Some(usize::MAX);
        settings.max_streams = Some(usize::MAX);
        settings.stream_buffer_bytes = Some(usize::MAX);
        settings.max_connections = Some(usize::MAX);
        settings.max_connections_per_ip = Some(usize::MAX);
        settings.max_subscriptions = Some(usize::MAX);
        settings.max_subscribers_per_plugin = Some(usize::MAX);
        settings.http_idle_timeout_secs = Some(u64::MAX);
        settings.log_max_bytes = Some(u64::MAX);
        settings.log_rotations = Some(usize::MAX);
        settings.max_package_bytes = Some(u64::MAX);
        settings.transfer_timeout_secs = Some(u64::MAX);
        settings.control_timeout_secs = Some(u64::MAX);
        settings.default_timeout_ms = Some(u64::MAX);
        settings.max_timeout_ms = Some(u64::MAX);
        settings.ws_max_message_bytes = Some(usize::MAX);
        settings.ws_inflight_bytes = Some(usize::MAX);
        settings.ws_inflight_requests = Some(usize::MAX);
        settings.ws_buffer_shrink_bytes = Some(usize::MAX);
        settings.ws_raw_max_bytes = Some(usize::MAX);
        settings.ws_raw_queue = Some(usize::MAX);

        let limits = limits_with(&settings);
        assert_eq!(limits.request_stack_bytes, 8 * 1024 * 1024);
        assert_eq!(limits.io_stack_bytes, 4 * 1024 * 1024);
        assert_eq!(limits.blocking_threads, 8);
        assert_eq!(limits.max_body, 1024 * 1024 * 1024);
        assert_eq!(limits.max_json_body, 64 * 1024 * 1024);
        assert_eq!(limits.max_complete_body, 128 * 1024 * 1024);
        assert_eq!(limits.max_header, 1024 * 1024);
        assert_eq!(limits.max_streams, 1024);
        assert_eq!(limits.stream_buffer, 1024 * 1024);
        assert_eq!(limits.max_connections, 4096);
        assert_eq!(limits.max_connections_per_ip, 1024);
        assert_eq!(limits.max_subscriptions, 4096);
        assert_eq!(limits.max_subscribers_per_plugin, 1024);
        assert_eq!(limits.idle_timeout, Duration::from_secs(3600));
        assert_eq!(limits.log_max_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.log_rotations, 10);
        assert_eq!(limits.max_package_bytes, 1024 * 1024 * 1024);
        assert_eq!(limits.transfer_timeout, Duration::from_secs(3600));
        assert_eq!(limits.control_timeout, Duration::from_secs(300));
        assert_eq!(limits.default_timeout, Duration::from_millis(300_000));
        assert_eq!(limits.max_timeout, Duration::from_millis(600_000));
        assert_eq!(limits.ws_max_message_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.ws_inflight_bytes, 1024 * 1024 * 1024);
        assert_eq!(limits.ws_inflight_requests, 4096);
        assert_eq!(limits.ws_buffer_shrink_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.ws_raw_max_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.ws_raw_queue, 4096);
    }

    #[test]
    fn blocking_threads_default_to_two_and_clamp_to_two_through_eight() {
        let defaults = limits_with(&settings::Settings::default());
        assert_eq!(defaults.blocking_threads, 2);

        for (configured, expected) in [(0, 2), (1, 2), (2, 2), (5, 5), (8, 8), (9, 8), (64, 8)] {
            let settings = settings::Settings {
                blocking_threads: Some(configured),
                ..settings::Settings::default()
            };
            assert_eq!(
                limits_with(&settings).blocking_threads,
                expected,
                "configured {configured}"
            );
        }
    }

    #[test]
    fn a_large_default_never_exceeds_the_max_timeout() {
        let mut settings = settings::Settings::default();
        settings.default_timeout_ms = Some(300_000);
        settings.max_timeout_ms = Some(1);
        let limits = limits_with(&settings);
        assert_eq!(limits.default_timeout, Duration::from_millis(300_000));
        assert_eq!(limits.max_timeout, Duration::from_millis(300_000));
    }

    #[test]
    fn defaults_match_the_selected_level() {
        let limits = Limits::from_settings(SettingsView::from(&settings::Settings::default()));
        assert!(limits.request_stack_bytes >= 64 * 1024);
        assert!(limits.io_stack_bytes >= 32 * 1024);
        assert!(limits.max_streams <= MAX_STREAMS);
        assert!(limits.log_max_bytes <= 16 * 1024 * 1024);
    }

    #[test]
    fn a_typo_cannot_go_below_the_floor() {
        let mut settings = settings::Settings::default();
        settings.max_streams = Some(0);
        settings.request_stack_bytes = Some(1);
        settings.io_stack_bytes = Some(1);
        let limits = Limits::from_settings(SettingsView::from(&settings));
        assert_eq!(limits.max_streams, 1);
        assert_eq!(limits.request_stack_bytes, 64 * 1024);
        assert_eq!(limits.io_stack_bytes, 32 * 1024);
    }
}
