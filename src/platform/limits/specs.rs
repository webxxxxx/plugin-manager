//! The derivation table: one `KeySpec` per derived key.
//!
//! Byte keys express their share as a fraction of the budget; count keys
//! express it as the budget one slot may claim. The three columns are the
//! reference budgets (128 MiB / 512 MiB / 2 GiB). The `high` ceiling is 8 GiB:
//! `auto` is a quarter of available memory, so a 2 GiB ceiling stopped tracking
//! the machine on any host with 8 GiB free (ADR-0026, 2026-11-21 修订三).

use crate::platform::resources::Workload;

/// The most this build may ever plan for. It only tightens: whatever the machine
/// reports and whatever the document asks for, the budget stays at or below it
/// (user stories 9 and 10).
#[cfg(mem_low)]
pub const BUDGET_CAP: u64 = 128 * 1024 * 1024;

#[cfg(mem_middle)]
pub const BUDGET_CAP: u64 = 512 * 1024 * 1024;

#[cfg(mem_high)]
pub const BUDGET_CAP: u64 = 8 * 1024 * 1024 * 1024;

pub(super) const MIB: u64 = 1024 * 1024;

/// What the process costs before any buffer: allocator, registry, the two
/// loops, log sinks.
///
/// Measured on the WSL reference host (2026-11-21, #107): an idle daemon with
/// no plugins settles at 3.4 MiB. The reserve is that measured floor with room
/// to spare, not the old 32 MiB guess that ate two thirds of a 50 MiB budget.
pub const RESIDENT_BASELINE_BYTES: u64 = 8 * MIB;

/// What one admitted connection costs in resident memory.
///
/// Measured on the same host: a plugin holding one WebSocket connection adds
/// 75-77 KiB to the daemon (100 -> 11.7 MiB, 200 -> 19.3, 300 -> 26.7,
/// 396 -> 33.7), and a plugin with no connection adds ~71 KiB, so most of this
/// is the supervision entry rather than the socket. The slot is charged the
/// larger of this floor and its worst-case buffers: buffers alone (29 KiB at
/// the tiny shape) under-state a real connection by more than half.
pub(super) const CONNECTION_RESIDENT_BYTES: u64 = 80 * 1024;

/// Threads that exist in every topology: the supervisor loop (which polls
/// plugin stdio) and the server loop.
pub(super) const LOOP_THREADS: usize = 2;

/// The invariant never shrinks concurrency below this. Eight clients is the
/// practical floor for a management service.
pub(super) const CONNECTION_FLOOR: usize = 8;

/// The control channel's frame limit. A queued management request holds at most
/// one frame.
pub(super) const IPC_FRAME_MAX: u64 = 64 * 1024;

/// Default blocking-pool workers at the interactive shape.
pub const DEFAULT_BLOCKING_THREADS: usize = 8;

/// A pool smaller than two workers cannot run a slow job and keep a second one
/// moving.
pub const MIN_BLOCKING_THREADS: usize = 2;

/// Upper bound so a typo cannot turn a small host into a thread farm. The
/// default scales with the shape instead of sitting at two: the pool runs the
/// management routes (install, upload, download, the API), and two workers
/// plus a 64-slot queue refused a fifty-client host that the memory budget
/// had already admitted.
pub const MAX_BLOCKING_THREADS: usize = 32;

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
    // tiny / interactive / streaming
    [4, DEFAULT_BLOCKING_THREADS as u64, 16],
);

/// Jobs the blocking pool may queue. The solved default is the smaller of the
/// connection count and 256; the invariant needs no term for it because a
/// queued job belongs to an already-admitted connection and its body was
/// already charged there.
pub(super) const BLOCKING_QUEUE: KeySpec = KeySpec::fixed("blocking_queue", 1, 1024, [8, 16, 24]);

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

/// Streams are cheap (one stream buffer each) and they are the realtime
/// admission path. The upper bound matches the connection ceiling: a UI page
/// holds one WS per plugin panel, so a stream cap below the connection count
/// makes most of the connection budget unreachable for the dominant realtime
/// workload (many plugin UIs, one stream each). The reference values stay the
/// shape defaults for an explicit smaller number.
pub(super) const MAX_STREAMS: KeySpec = KeySpec::fixed("max_streams", 1, 4096, [24, 48, 96]);

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
/// The connection count is solved against the budget instead of being a share
/// of it (see `Limits::fitting_connections`): one slot's worst case is tens of
/// kilobytes, not megabytes, so a share only ever under-filled the budget.
/// This spec bounds an explicit value; an unconfigured one starts at the
/// ceiling and is replaced by the solved count before the invariant runs.
pub(super) const MAX_CONNECTIONS: KeySpec =
    KeySpec::fixed("max_connections", 1, 4096, [4096, 4096, 4096]);

pub(super) const MAX_CONNECTIONS_PER_IP: KeySpec =
    KeySpec::fixed("max_connections_per_ip", 1, 1024, [16, 32, 64]);

/// One subscriber costs a cursor, not a queue: the bound lives in the single
/// UI ring (see UI_RING_BYTES), so the count can follow the stream cap.
pub(super) const MAX_SUBSCRIPTIONS: KeySpec =
    KeySpec::fixed("max_subscriptions", 0, 4096, [64, 128, 256]);

pub(super) const MAX_SUBSCRIBERS_PER_PLUGIN: KeySpec =
    KeySpec::fixed("max_subscribers_per_plugin", 0, 1024, [16, 32, 64]);

/// Share of the machine's CPU the daemon may use. 0 disables the governor.
/// Unlike the memory keys this is a live policy, not a reservation: the
/// governor sheds new connections once its own measured use stays above it.
pub(super) const CPU_BUDGET_PERCENT: KeySpec =
    KeySpec::fixed("cpu_budget_percent", 0, 100, [0, 0, 0]);

pub(super) const IDLE_TIMEOUT: KeySpec =
    KeySpec::fixed("http_idle_timeout_secs", 1, 3600, [30, 60, 120]);

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

pub(super) const CONTROL_TIMEOUT: KeySpec =
    KeySpec::fixed("control_timeout_secs", 1, 300, [30, 30, 30]);

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

pub(super) const WS_RAW_MAX: KeySpec =
    KeySpec::fixed("ws_raw_max_bytes", 1024, 16 * MIB, [MIB, MIB, MIB]);

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
