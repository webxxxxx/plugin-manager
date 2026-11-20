//! Compile-time budget ceiling, workload shape and the derivation table.
//!
//! The old `MEM_LEVEL` picked a table of absolute defaults. It now only caps the
//! **memory budget** (see [`crate::platform::resources`]): the machine decides how
//! much there is, the cap decides how much this build may ever claim, and the
//! workload shape decides how the budget is spent.
//!
//! `svcd.conf` may still name any key. A value inside
//! `[absolute floor, min(share × budget, absolute ceiling)]` is taken as
//! written; anything outside it is rewritten and logged as an
//! [`Override`] instead of disappearing silently.

use std::fmt;
use std::time::Duration;

use crate::platform::resources::{budget, Budget, ResourceProfile, Workload, WorkloadSource};
use crate::platform::settings;

/// The most this build may ever plan for. It only tightens: whatever the machine
/// reports and whatever the document asks for, the budget stays at or below it
/// (user stories 9 and 10).
#[cfg(mem_low)]
pub const BUDGET_CAP: u64 = 128 * 1024 * 1024;
#[cfg(mem_middle)]
pub const BUDGET_CAP: u64 = 512 * 1024 * 1024;
#[cfg(mem_high)]
pub const BUDGET_CAP: u64 = 2 * 1024 * 1024 * 1024;

pub(super) const MIB: u64 = 1024 * 1024;

/// What the process costs before any buffer: allocator, registry, the two
/// loops, log sinks. Deliberately pessimistic; a measured baseline replaces it
/// in #107.
pub const RESIDENT_BASELINE_BYTES: u64 = 32 * MIB;

/// Threads that exist in every topology: the supervisor loop (which polls
/// plugin stdio) and the server loop.
pub(super) const LOOP_THREADS: usize = 2;

/// The invariant never shrinks concurrency below this. Eight clients is the
/// practical floor for a management service.
pub(super) const CONNECTION_FLOOR: usize = 8;

/// The control channel's frame limit. A queued management request holds at most
/// one frame.
pub(super) const IPC_FRAME_MAX: u64 = 64 * 1024;

/// Default blocking-pool workers.
pub const DEFAULT_BLOCKING_THREADS: usize = 2;
/// A pool smaller than two workers cannot run a slow job and keep a second one
/// moving.
pub const MIN_BLOCKING_THREADS: usize = 2;
/// Upper bound so a typo cannot turn a small host into a thread farm.
pub const MAX_BLOCKING_THREADS: usize = 8;

/// A fraction of the quantized budget: `budget * num / den`.
///
/// One representation for both byte keys (`num/den` of the budget) and count
/// keys (`num = 1`, `den` = the budget one slot may claim), so the table has a
/// single shape and the arithmetic cannot drift between the two.
#[derive(Debug, Clone, Copy)]
struct Share {
    num: u64,
    den: u64,
}

impl Share {
    const fn of(num: u64, den: u64) -> Share {
        Share { num, den }
    }

    /// One slot per `unit` bytes of budget.
    const fn per(unit: u64) -> Share {
        Share::of(1, unit)
    }

    fn at(self, budget: u64) -> u64 {
        budget.saturating_mul(self.num) / self.den
    }
}

/// How the budget moves a key.
#[derive(Debug, Clone, Copy)]
enum Basis {
    /// The value is a fraction of the budget, so it follows both the machine and
    /// the shape.
    Share([Share; 3]),
    /// The budget does not move it; the shape alone picks the value. Timeouts,
    /// thread stacks and log sizes must not scale with RAM.
    Fixed([u64; 3]),
}

/// One row of the derivation table.
#[derive(Debug, Clone, Copy)]
pub(super) struct KeySpec {
    pub(super) key: &'static str,
    pub(super) min: u64,
    max: u64,
    basis: Basis,
}

impl KeySpec {
    const fn share(key: &'static str, min: u64, max: u64, shares: [Share; 3]) -> KeySpec {
        KeySpec {
            key,
            min,
            max,
            basis: Basis::Share(shares),
        }
    }

    const fn fixed(key: &'static str, min: u64, max: u64, values: [u64; 3]) -> KeySpec {
        KeySpec {
            key,
            min,
            max,
            basis: Basis::Fixed(values),
        }
    }

    /// The value this key gets when the document does not name it: the shape's
    /// share applied to the budget actually in force.
    pub(super) fn default_value(&self, workload: Workload, budget: u64) -> u64 {
        let raw = match self.basis {
            Basis::Share(shares) => shares[workload.index()].at(budget),
            Basis::Fixed(values) => values[workload.index()],
        };
        raw.clamp(self.min, self.max)
    }

    /// Where an over-the-line value falls back to: this shape's expectation
    /// at the budget actually in force, not the ceiling and not the number a
    /// larger build would have used. A typo therefore returns to a sane
    /// number rather than to the largest one that happens to fit.
    pub(super) fn fallback_value(&self, workload: Workload, budget: u64) -> u64 {
        self.default_value(workload, budget)
    }

    /// The largest value a document may name and still be taken as written.
    pub(super) fn upper_bound(&self, workload: Workload, budget: u64) -> u64 {
        match self.basis {
            Basis::Share(shares) => shares[workload.index()]
                .at(budget)
                .clamp(self.min, self.max),
            Basis::Fixed(_) => self.max,
        }
    }
}

/// Every limit svcd derives. See [`derive`].
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
    /// Bound for one management route's control-channel round trip.
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
    /// Queued management requests between the server loop and the supervisor.
    pub pending_queue: usize,
    /// Progress events queued between the reactor and one MCP stream.
    pub progress_queue: usize,
}

/// Why a value in the document did not become the value in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideReason {
    /// Above `min(share × budget, absolute ceiling)`: back to the shape's
    /// reference expectation.
    AboveUpperBound,
    /// Below the absolute floor: raised to it.
    BelowFloor,
    /// The worst-case invariant did not fit, so the value was lowered.
    InvariantShrink,
    /// The build's budget ceiling tightened the configured budget.
    BuildCap,
}

impl OverrideReason {
    pub fn as_str(self) -> &'static str {
        match self {
            OverrideReason::AboveUpperBound => "above-upper-bound",
            OverrideReason::BelowFloor => "below-floor",
            OverrideReason::InvariantShrink => "invariant-shrink",
            OverrideReason::BuildCap => "build-cap",
        }
    }
}

/// One readable rewrite: `key: configured -> effective (reason)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    pub key: &'static str,
    pub configured: u64,
    pub effective: u64,
    pub reason: OverrideReason,
}

impl Override {
    pub fn new(
        key: &'static str,
        configured: u64,
        effective: u64,
        reason: OverrideReason,
    ) -> Override {
        Override {
            key,
            configured,
            effective,
            reason,
        }
    }
}

impl fmt::Display for Override {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "{}: {} -> {} ({})",
            self.key,
            self.configured,
            self.effective,
            self.reason.as_str()
        )
    }
}

/// Everything the composition root derives along with the [`Limits`]: what the
/// machine reported, how much the process may plan for, how it spends it, and
/// every rewrite the derivation performed.
#[derive(Debug, Clone)]
pub struct ResourceReport {
    pub profile: ResourceProfile,
    pub budget: Budget,
    pub workload: Workload,
    pub workload_source: WorkloadSource,
    /// [`BUDGET_CAP`] for this build.
    pub cap: u64,
    /// `worst_case` of the effective limits; always `<= budget.budget` when
    /// `refusal` is `None`.
    pub worst_case: u64,
    pub notes: Vec<Override>,
    /// Why the derived configuration cannot run at all. The daemon refuses to
    /// start and prints this.
    pub refusal: Option<String>,
}

impl ResourceReport {
    /// The budget in force: the quantized number every share is applied to.
    pub fn budget_bytes(&self) -> u64 {
        self.budget.quantized
    }

    /// The four fields the daemon logs at startup, one per line.
    pub fn lines(&self) -> Vec<String> {
        describe_snapshot(&self.to_json())
    }

    /// The snapshot the daemon writes at startup and `svc status` reads back.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "available_memory": self.profile.available_memory,
            "total_memory": self.profile.total_memory,
            "cgroup_remaining": self.profile.cgroup_remaining,
            "cpus": self.profile.cpus,
            "install_free_disk": self.profile.install_free_disk,
            "temp_free_disk": self.profile.temp_free_disk,
            "budget_raw": self.budget.raw,
            "budget_quantized": self.budget.quantized,
            "budget_source": self.budget.source.as_str(),
            "build_cap": self.cap,
            "workload": self.workload.as_str(),
            "workload_source": self.workload_source.as_str(),
            "worst_case": self.worst_case,
            "refusal": self.refusal,
            "notes": self
                .notes
                .iter()
                .map(|note| {
                    serde_json::json!({
                        "key": note.key,
                        "configured": note.configured,
                        "effective": note.effective,
                        "reason": note.reason.as_str(),
                    })
                })
                .collect::<Vec<_>>(),
        })
    }
}

/// The derived value of everything, plus the report that explains it.
#[derive(Debug, Clone)]
pub struct Derived {
    pub limits: Limits,
    pub report: ResourceReport,
}

/// Derive from a document with no machine description: the budget is the build
/// ceiling and the workload follows it.
///
/// The composition root uses [`derive_with`] and passes the probe. A unit test
/// that only needs sane limits uses this one, which keeps it independent of the
/// machine it runs on.
#[cfg(test)]
pub fn derive(settings: &settings::Settings) -> Derived {
    derive_with(settings, &ResourceProfile::default())
}

/// Derive the limits from a document and a machine description.
///
/// Pure: same document plus same profile, same result (user story 14).
pub fn derive_with(settings: &settings::Settings, profile: &ResourceProfile) -> Derived {
    let budget = budget(settings.mem_budget_bytes, profile, BUDGET_CAP);
    let workload = settings
        .workload
        .unwrap_or_else(|| Workload::suggested(budget.quantized, profile.cpus));
    let workload_source = match settings.workload {
        Some(_) => WorkloadSource::Configured,
        None => WorkloadSource::Derived,
    };

    let mut notes = Vec::new();
    // The ceiling only tightens, and the floor only raises: both are rewrites of
    // the number the document named, so both are reported.
    if let Some(configured) = settings.mem_budget_bytes {
        if configured > BUDGET_CAP {
            notes.push(Override::new(
                "mem_budget_bytes",
                configured,
                BUDGET_CAP,
                OverrideReason::BuildCap,
            ));
        } else if configured < budget.raw {
            notes.push(Override::new(
                "mem_budget_bytes",
                configured,
                budget.raw,
                OverrideReason::BelowFloor,
            ));
        }
    }

    let mut derivation = Derivation {
        workload,
        budget: budget.quantized,
        notes: &mut notes,
    };
    let mut limits = Limits::from_settings(SettingsView::from(settings), &mut derivation);
    let refusal = apply_invariant(&mut limits, budget.quantized, &mut notes);
    let worst_case = limits.worst_case_bytes();
    Derived {
        limits,
        report: ResourceReport {
            profile: profile.clone(),
            budget,
            workload,
            workload_source,
            cap: BUDGET_CAP,
            worst_case,
            notes,
            refusal,
        },
    }
}

/// The limits an unconfigured process would run with. Unit tests use this
/// instead of re-deriving from `Settings::default()` at every seam.
#[cfg(test)]
pub(crate) fn test_default() -> Limits {
    derive(&settings::Settings::default()).limits
}

struct Derivation<'a> {
    workload: Workload,
    budget: u64,
    notes: &'a mut Vec<Override>,
}

impl Derivation<'_> {
    /// The effective value of one key, recording every rewrite.
    fn value(&mut self, spec: &KeySpec, configured: Option<u64>) -> u64 {
        let default = spec.default_value(self.workload, self.budget);
        let Some(configured) = configured else {
            return default;
        };
        let upper = spec.upper_bound(self.workload, self.budget);
        if configured > upper {
            let effective = spec.fallback_value(self.workload, self.budget);
            self.notes.push(Override::new(
                spec.key,
                configured,
                effective,
                OverrideReason::AboveUpperBound,
            ));
            return effective;
        }
        if configured < spec.min {
            self.notes.push(Override::new(
                spec.key,
                configured,
                spec.min,
                OverrideReason::BelowFloor,
            ));
            return spec.min;
        }
        configured
    }

    fn bytes(&mut self, spec: &KeySpec, configured: Option<u64>) -> u64 {
        self.value(spec, configured)
    }

    fn byte_count(&mut self, spec: &KeySpec, configured: Option<usize>) -> usize {
        self.value(spec, configured.map(|value| value as u64)) as usize
    }

    fn count(&mut self, spec: &KeySpec, configured: Option<u64>) -> usize {
        self.value(spec, configured) as usize
    }

    fn secs(&mut self, spec: &KeySpec, configured: Option<u64>) -> Duration {
        Duration::from_secs(self.value(spec, configured))
    }

    fn millis(&mut self, spec: &KeySpec, configured: Option<u64>) -> Duration {
        Duration::from_millis(self.value(spec, configured))
    }
}

// ---------------------------------------------------------------------------
// The derivation table.
//
// Byte keys express their share as a fraction of the budget; count keys express
// it as the budget one slot may claim. The three columns are the reference
// budgets (128 MiB / 512 MiB / 2 GiB), so `default_value` at a reference budget
// reproduces the historical table within one order of magnitude.
// ---------------------------------------------------------------------------

pub(super) const REQUEST_STACK: KeySpec = KeySpec::fixed(
    "request_stack_bytes",
    64 * 1024,
    8 * MIB,
    [128 * 1024, 192 * 1024, 256 * 1024],
);
pub(super) const IO_STACK: KeySpec = KeySpec::fixed(
    "io_stack_bytes",
    32 * 1024,
    4 * MIB,
    [64 * 1024, 96 * 1024, 128 * 1024],
);
pub(super) const BLOCKING_THREADS: KeySpec = KeySpec::fixed(
    "blocking_threads",
    MIN_BLOCKING_THREADS as u64,
    MAX_BLOCKING_THREADS as u64,
    [
        DEFAULT_BLOCKING_THREADS as u64,
        DEFAULT_BLOCKING_THREADS as u64,
        DEFAULT_BLOCKING_THREADS as u64,
    ],
);
/// The wire-level request cap. Complete bodies beyond `max_complete_body` are
/// spooled, so this one is a protocol bound and not a resident buffer.
pub(super) const MAX_BODY: KeySpec = KeySpec::share(
    "max_body_bytes",
    64 * 1024,
    1024 * MIB,
    [Share::of(1, 16), Share::of(1, 8), Share::of(1, 4)],
);
pub(super) const MAX_JSON_BODY: KeySpec = KeySpec::share(
    "max_json_body_bytes",
    4 * 1024,
    64 * MIB,
    [Share::of(1, 2048), Share::of(1, 2048), Share::of(1, 2048)],
);
/// The per-connection resident body: this is the term the invariant charges for
/// every connection. It must stay small enough that connections × it fits.
pub(super) const MAX_COMPLETE_BODY: KeySpec = KeySpec::share(
    "max_complete_body_bytes",
    16 * 1024,
    128 * MIB,
    [Share::of(1, 2048), Share::of(1, 512), Share::of(1, 1024)],
);
pub(super) const MAX_HEADER: KeySpec = KeySpec::fixed(
    "max_header_bytes",
    1024,
    MIB,
    [8 * 1024, 16 * 1024, 64 * 1024],
);
pub(super) const MAX_STREAMS: KeySpec = KeySpec::fixed("max_streams", 1, 1024, [8, 16, 64]);
pub(super) const STREAM_BUFFER: KeySpec = KeySpec::share(
    "stream_buffer_bytes",
    4 * 1024,
    MIB,
    [
        Share::of(1, 16384),
        Share::of(1, 16384),
        Share::of(1, 16384),
    ],
);
/// One connection slot may claim this much budget. The three shares are strictly
/// ordered, so the shapes stay distinguishable at any budget.
pub(super) const MAX_CONNECTIONS: KeySpec = KeySpec::share(
    "max_connections",
    1,
    4096,
    [
        Share::per(16 * MIB / 3),
        Share::per(4 * MIB),
        Share::per(3 * MIB),
    ],
);
pub(super) const MAX_CONNECTIONS_PER_IP: KeySpec =
    KeySpec::fixed("max_connections_per_ip", 1, 1024, [8, 32, 64]);
pub(super) const MAX_SUBSCRIPTIONS: KeySpec = KeySpec::fixed("max_subscriptions", 0, 4096, [4, 16, 64]);
pub(super) const MAX_SUBSCRIBERS_PER_PLUGIN: KeySpec =
    KeySpec::fixed("max_subscribers_per_plugin", 0, 1024, [4, 16, 64]);
pub(super) const IDLE_TIMEOUT: KeySpec = KeySpec::fixed("http_idle_timeout_secs", 1, 3600, [30, 60, 120]);
pub(super) const LOG_MAX_BYTES: KeySpec = KeySpec::fixed(
    "log_max_bytes",
    4 * 1024,
    16 * MIB,
    [64 * 1024, 64 * 1024, 64 * 1024],
);
pub(super) const LOG_ROTATIONS: KeySpec = KeySpec::fixed("log_rotations", 1, 10, [3, 3, 3]);
pub(super) const MAX_PACKAGE_BYTES: KeySpec = KeySpec::fixed(
    "max_package_bytes",
    MIB,
    1024 * MIB,
    [8 * MIB, 64 * MIB, 512 * MIB],
);
pub(super) const TRANSFER_TIMEOUT: KeySpec =
    KeySpec::fixed("transfer_timeout_secs", 10, 3600, [120, 120, 120]);
pub(super) const CONTROL_TIMEOUT: KeySpec = KeySpec::fixed("control_timeout_secs", 1, 300, [30, 30, 30]);
pub(super) const DEFAULT_TIMEOUT: KeySpec =
    KeySpec::fixed("default_timeout", 100, 300_000, [30_000, 30_000, 30_000]);
pub(super) const MAX_TIMEOUT: KeySpec =
    KeySpec::fixed("max_timeout", 100, 600_000, [300_000, 300_000, 300_000]);
pub(super) const WS_MAX_MESSAGE: KeySpec =
    KeySpec::fixed("ws_max_message_bytes", 1024, 16 * MIB, [MIB, MIB, MIB]);
pub(super) const WS_INFLIGHT_BYTES: KeySpec = KeySpec::share(
    "ws_inflight_bytes",
    64 * 1024,
    1024 * MIB,
    [Share::of(1, 64), Share::of(1, 64), Share::of(1, 32)],
);
pub(super) const WS_INFLIGHT_REQUESTS: KeySpec = KeySpec::share(
    "ws_inflight_requests",
    1,
    4096,
    [
        Share::per(32 * MIB),
        Share::per(32 * MIB),
        Share::per(16 * MIB),
    ],
);
pub(super) const WS_BUFFER_SHRINK: KeySpec = KeySpec::fixed(
    "ws_buffer_shrink_bytes",
    4 * 1024,
    16 * MIB,
    [64 * 1024, 64 * 1024, 64 * 1024],
);
pub(super) const WS_RAW_MAX: KeySpec = KeySpec::fixed("ws_raw_max_bytes", 1024, 16 * MIB, [MIB, MIB, MIB]);
pub(super) const WS_RAW_QUEUE: KeySpec = KeySpec::share(
    "ws_raw_queue",
    4,
    4096,
    [
        Share::per(16 * MIB),
        Share::per(16 * MIB),
        Share::per(16 * MIB),
    ],
);
pub(super) const PENDING_QUEUE: KeySpec = KeySpec::share(
    "pending_queue",
    8,
    4096,
    [
        Share::per(4 * MIB),
        Share::per(4 * MIB),
        Share::per(4 * MIB),
    ],
);
pub(super) const PROGRESS_QUEUE: KeySpec = KeySpec::share(
    "progress_queue",
    4,
    1024,
    [
        Share::per(8 * MIB),
        Share::per(8 * MIB),
        Share::per(8 * MIB),
    ],
);

/// Everything a derivation needs from the document.
struct SettingsView {
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
    pending_queue: Option<usize>,
    progress_queue: Option<usize>,
}

impl SettingsView {
    fn from(settings: &settings::Settings) -> Self {
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
            pending_queue: settings.pending_queue,
            progress_queue: settings.progress_queue,
        }
    }
}

impl Limits {
    fn from_settings(settings: SettingsView, derivation: &mut Derivation<'_>) -> Limits {
        let request_stack_bytes =
            derivation.byte_count(&REQUEST_STACK, settings.request_stack_bytes);
        let io_stack_bytes = derivation.byte_count(&IO_STACK, settings.io_stack_bytes);
        let blocking_threads = derivation.count(
            &BLOCKING_THREADS,
            settings.blocking_threads.map(|value| value as u64),
        );
        let max_body = derivation.bytes(&MAX_BODY, settings.max_body_bytes);
        let max_json_body = derivation.bytes(&MAX_JSON_BODY, settings.max_json_body_bytes);
        let max_complete_body =
            derivation.bytes(&MAX_COMPLETE_BODY, settings.max_complete_body_bytes);
        let max_header = derivation.byte_count(&MAX_HEADER, settings.max_header_bytes);
        let max_streams = derivation.count(&MAX_STREAMS, settings.max_streams.map(|v| v as u64));
        let stream_buffer = derivation.byte_count(&STREAM_BUFFER, settings.stream_buffer_bytes);
        let max_connections =
            derivation.count(&MAX_CONNECTIONS, settings.max_connections.map(|v| v as u64));
        let max_connections_per_ip = derivation.count(
            &MAX_CONNECTIONS_PER_IP,
            settings.max_connections_per_ip.map(|v| v as u64),
        );
        let max_subscriptions = derivation.count(
            &MAX_SUBSCRIPTIONS,
            settings.max_subscriptions.map(|v| v as u64),
        );
        let max_subscribers_per_plugin = derivation.count(
            &MAX_SUBSCRIBERS_PER_PLUGIN,
            settings.max_subscribers_per_plugin.map(|v| v as u64),
        );
        let idle_timeout = derivation.secs(&IDLE_TIMEOUT, settings.idle_timeout_secs);
        let log_max_bytes = derivation.bytes(&LOG_MAX_BYTES, settings.log_max_bytes);
        let log_rotations =
            derivation.count(&LOG_ROTATIONS, settings.log_rotations.map(|v| v as u64));
        let log_to_file = settings.log_to_file.unwrap_or(true);
        let max_package_bytes = derivation.bytes(&MAX_PACKAGE_BYTES, settings.max_package_bytes);
        let transfer_timeout = derivation.secs(&TRANSFER_TIMEOUT, settings.transfer_timeout_secs);
        let control_timeout = derivation.secs(&CONTROL_TIMEOUT, settings.control_timeout_secs);
        let default_timeout = derivation.millis(&DEFAULT_TIMEOUT, settings.default_timeout_ms);
        // The max timeout's floor is dynamic: it can never be below the default,
        // otherwise the envelope would advertise an unusable pair.
        let configured_max = settings.max_timeout_ms;
        let mut max_timeout = derivation.millis(&MAX_TIMEOUT, configured_max);
        if max_timeout < default_timeout {
            max_timeout = default_timeout;
            if let Some(configured) = configured_max {
                derivation.notes.push(Override::new(
                    MAX_TIMEOUT.key,
                    configured,
                    default_timeout.as_millis() as u64,
                    OverrideReason::BelowFloor,
                ));
            }
        }
        let ws_max_message_bytes =
            derivation.byte_count(&WS_MAX_MESSAGE, settings.ws_max_message_bytes);
        let ws_inflight_bytes =
            derivation.byte_count(&WS_INFLIGHT_BYTES, settings.ws_inflight_bytes);
        let ws_inflight_requests = derivation.count(
            &WS_INFLIGHT_REQUESTS,
            settings.ws_inflight_requests.map(|v| v as u64),
        );
        let ws_buffer_shrink_bytes =
            derivation.byte_count(&WS_BUFFER_SHRINK, settings.ws_buffer_shrink_bytes);
        let ws_raw_enabled = settings.ws_raw_enabled.unwrap_or(true);
        let ws_raw_max_bytes = derivation.byte_count(&WS_RAW_MAX, settings.ws_raw_max_bytes);
        let ws_raw_queue = derivation.count(&WS_RAW_QUEUE, settings.ws_raw_queue.map(|v| v as u64));
        let pending_queue =
            derivation.count(&PENDING_QUEUE, settings.pending_queue.map(|v| v as u64));
        let progress_queue =
            derivation.count(&PROGRESS_QUEUE, settings.progress_queue.map(|v| v as u64));

        Limits {
            request_stack_bytes,
            io_stack_bytes,
            blocking_threads,
            max_body,
            max_json_body,
            max_complete_body,
            max_header,
            max_streams,
            stream_buffer,
            max_connections,
            max_connections_per_ip,
            max_subscriptions,
            max_subscribers_per_plugin,
            idle_timeout,
            log_max_bytes,
            log_rotations,
            log_to_file,
            max_package_bytes,
            transfer_timeout,
            control_timeout,
            default_timeout,
            max_timeout,
            ws_max_message_bytes,
            ws_inflight_bytes,
            ws_inflight_requests,
            ws_buffer_shrink_bytes,
            ws_raw_enabled,
            ws_raw_max_bytes,
            ws_raw_queue,
            pending_queue,
            progress_queue,
        }
    }
}

/// Threads the invariant charges for a stack.
///
/// Unix runs the supervisor loop, the server loop and the blocking pool; the
/// per-connection HTTP fallback does not exist there. Windows keeps one thread
/// per admitted connection, so the connection cap *is* the thread cap and
/// `min(derived threads, max_connections)` is simply `max_connections`.
fn threads_for(max_connections: usize, blocking_threads: usize) -> usize {
    if cfg!(windows) {
        max_connections
    } else {
        LOOP_THREADS + blocking_threads
    }
}

impl Limits {
    /// Threads the invariant charges a stack for.
    pub fn threads(&self) -> usize {
        threads_for(self.max_connections, self.blocking_threads)
    }

    /// The peak memory this configuration may reach, by the spec's formula:
    ///
    /// ```text
    /// baseline
    ///   + threads × request stack
    ///   + connections × (resident single request + stream buffer)
    ///   + streams × stream buffer
    ///   + in-flight raw bytes
    ///   + queued frames × frame limit
    /// ```
    ///
    /// Every term is something that can be live at the same time, which is what
    /// makes "this machine will use at most X" readable from the configuration
    /// (user story 15).
    pub fn worst_case_bytes(&self) -> u64 {
        let threads = self.threads() as u64 * self.request_stack_bytes as u64;
        let connections =
            self.max_connections as u64 * (self.max_complete_body + self.stream_buffer as u64);
        let streams = self.max_streams as u64 * self.stream_buffer as u64;
        let inflight = self.ws_inflight_bytes as u64;
        let queues = (self.pending_queue as u64 + self.progress_queue as u64) * IPC_FRAME_MAX
            + self.ws_raw_queue as u64 * self.ws_raw_max_bytes as u64;
        RESIDENT_BASELINE_BYTES
            .saturating_add(threads)
            .saturating_add(connections)
            .saturating_add(streams)
            .saturating_add(inflight)
            .saturating_add(queues)
    }

    /// One row per term of [`Self::worst_case_bytes`], for the refusal message.
    fn worst_case_terms(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("resident baseline", RESIDENT_BASELINE_BYTES),
            (
                "thread stacks",
                self.threads() as u64 * self.request_stack_bytes as u64,
            ),
            (
                "connection buffers",
                self.max_connections as u64 * (self.max_complete_body + self.stream_buffer as u64),
            ),
            (
                "stream buffers",
                self.max_streams as u64 * self.stream_buffer as u64,
            ),
            ("raw in-flight bytes", self.ws_inflight_bytes as u64),
            (
                "queued frames",
                (self.pending_queue as u64 + self.progress_queue as u64) * IPC_FRAME_MAX
                    + self.ws_raw_queue as u64 * self.ws_raw_max_bytes as u64,
            ),
        ]
    }
}

/// Check `worst_case(shape, budget) <= budget` and shrink until it holds.
///
/// The order is fixed by the spec: shrink the single-request buffers first,
/// then the concurrency, then refuse to start. Every step records an
/// [`Override`], so the operator sees what changed even when they never named
/// the key.
///
/// The single-request shrink is solved, not floored: a configuration that is
/// one connection's worth over its budget loses exactly that much body, so the
/// shipped defaults stay at their derived values instead of collapsing to the
/// smallest legal body the first time the invariant binds (user story 24).
fn apply_invariant(limits: &mut Limits, budget: u64, notes: &mut Vec<Override>) -> Option<String> {
    if limits.worst_case_bytes() <= budget {
        return None;
    }

    // 1. The resident single-request body, which is the only per-connection
    //    buffer the invariant charges for.
    let configured_body = limits.max_complete_body;
    if configured_body > MAX_COMPLETE_BODY.min {
        let stream_buffer = limits.stream_buffer as u64;
        let others = limits.worst_case_bytes()
            - limits.max_connections as u64 * (configured_body + stream_buffer);
        let per_connection = budget.saturating_sub(others) / limits.max_connections.max(1) as u64;
        let body = per_connection
            .saturating_sub(stream_buffer)
            .clamp(MAX_COMPLETE_BODY.min, configured_body);
        if body < configured_body {
            notes.push(Override::new(
                MAX_COMPLETE_BODY.key,
                configured_body,
                body,
                OverrideReason::InvariantShrink,
            ));
            limits.max_complete_body = body;
        }
    }
    if limits.worst_case_bytes() <= budget {
        return None;
    }

    // 2. Concurrency, down to the floor. The stack term follows the connection
    //    count where the host runs one thread per connection, so the per-slot
    //    cost is not just the buffers.
    let configured_connections = limits.max_connections;
    let floor = CONNECTION_FLOOR.min(configured_connections);
    let per_connection = (limits.max_complete_body + limits.stream_buffer as u64)
        .saturating_add(stack_per_connection(limits));
    let others = limits.worst_case_bytes() - limits.max_connections as u64 * per_connection.max(1);
    let fits = budget.saturating_sub(others) / per_connection.max(1);
    let next = (fits as usize).clamp(floor, configured_connections);
    if next < configured_connections {
        notes.push(Override::new(
            MAX_CONNECTIONS.key,
            configured_connections as u64,
            next as u64,
            OverrideReason::InvariantShrink,
        ));
        limits.max_connections = next;
        limits.max_connections_per_ip = limits.max_connections_per_ip.min(next);
    }
    if limits.worst_case_bytes() <= budget {
        return None;
    }

    // 3. Nothing left to shrink: say exactly what does not fit.
    let worst = limits.worst_case_bytes();
    let terms = limits
        .worst_case_terms()
        .into_iter()
        .map(|(name, bytes)| format!("{} {}", name, crate::platform::resources::bytes_text(bytes)))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "the worst-case memory invariant does not fit the {} memory budget: \
         needs {} even with single-request buffers and concurrency at their \
         floors ({})",
        crate::platform::resources::bytes_text(budget),
        crate::platform::resources::bytes_text(worst),
        terms
    ))
}

/// The stack a connection adds on a host where connections own threads.
fn stack_per_connection(limits: &Limits) -> u64 {
    if cfg!(windows) {
        // Charged again per connection by `threads_for`, so the solver must
        // account for it or it would stop one iteration too early.
        limits.request_stack_bytes as u64
    } else {
        0
    }
}

/// The file the daemon writes at startup and `svc status --resources` reads
/// back.
///
/// It lives under the run directory in its own subdirectory: `run/` also holds
/// one `<id>.json` runtime record per service, and those are scanned as
/// `*.json`, so the snapshot must not be a sibling of them. It describes the
/// last start, it is not a second configuration source, and it is allowed to
/// disappear on reboot.
pub fn snapshot_path(paths: &crate::platform::paths::Paths) -> std::path::PathBuf {
    paths.run_dir.join("resources").join("startup.json")
}

/// Persist the report of the current start.
///
/// Written through a temporary file in the same directory and renamed, so a
/// reader never sees a half-written document.
pub fn write_snapshot(
    paths: &crate::platform::paths::Paths,
    report: &ResourceReport,
) -> Result<(), String> {
    let path = snapshot_path(paths);
    let directory = path
        .parent()
        .ok_or_else(|| format!("no directory for {}", path.display()))?;
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("create {}: {}", directory.display(), error))?;
    let text = serde_json::to_string_pretty(&report.to_json())
        .map_err(|error| format!("serialize the resource report: {}", error))?;
    let temporary = directory.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("resources.json"),
        std::process::id()
    ));
    std::fs::write(&temporary, text)
        .map_err(|error| format!("write {}: {}", temporary.display(), error))?;
    std::fs::rename(&temporary, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("rename {}: {}", path.display(), error)
    })
}

/// Read the snapshot of the last start.
pub fn read_snapshot(paths: &crate::platform::paths::Paths) -> Result<serde_json::Value, String> {
    let path = snapshot_path(paths);
    let text = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "no resource snapshot at {} ({error}); start the daemon first",
            path.display()
        )
    })?;
    serde_json::from_str(&text).map_err(|error| format!("parse {}: {}", path.display(), error))
}

/// Render the startup snapshot (live or from the file `svc status
/// --resources` reads) as the lines the daemon logs.
pub fn describe_snapshot(snapshot: &serde_json::Value) -> Vec<String> {
    let bytes = |key: &str| snapshot.get(key).and_then(serde_json::Value::as_u64);
    let text = crate::platform::resources::bytes_text;
    let show = |key: &str| bytes(key).map(text).unwrap_or_else(|| "unknown".into());
    let mut free = show("available_memory");
    if let Some(remaining) = bytes("cgroup_remaining") {
        free.push_str(&format!(", cgroup {}", text(remaining)));
    }
    let cpus = bytes("cpus").map(|cpus| format!(", {cpus} cpus"));
    let mut lines = vec![format!(
        "probe:    free memory {}{}",
        free,
        cpus.unwrap_or_default()
    )];
    lines.push(format!(
        "memory:   {} usable, from {} raw ({}), build cap {}",
        show("budget_quantized"),
        show("budget_raw"),
        snapshot
            .get("budget_source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
        show("build_cap")
    ));
    lines.push(format!(
        "workload: {} ({})",
        snapshot
            .get("workload")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown"),
        snapshot
            .get("workload_source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
    ));
    lines.push(format!(
        "worst case: {} of {}",
        show("worst_case"),
        show("budget_quantized")
    ));
    if let Some(reason) = snapshot
        .get("refusal")
        .and_then(serde_json::Value::as_str)
        .filter(|reason| !reason.is_empty())
    {
        lines.push(format!("refused: {}", reason));
    }
    match snapshot
        .get("notes")
        .and_then(serde_json::Value::as_array)
        .filter(|notes| !notes.is_empty())
    {
        None => lines.push("overrides: none".into()),
        Some(notes) => {
            for note in notes {
                lines.push(format!(
                    "override: {}: {} -> {} ({})",
                    note.get("key")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?"),
                    note.get("configured")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    note.get("effective")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    note.get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?")
                ));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests;
