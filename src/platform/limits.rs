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

use std::time::Duration;

#[cfg(test)]
use crate::platform::resources::{ResourceProfile, Workload, WorkloadSource};

mod derive;
mod report;
mod specs;

use specs::*;

#[allow(unused_imports)] // `limits::…` stays the public path after the split.
pub use specs::{
    BUDGET_CAP, DEFAULT_BLOCKING_THREADS, MAX_BLOCKING_THREADS, MIN_BLOCKING_THREADS,
    RESIDENT_BASELINE_BYTES,
};

#[cfg(test)]
pub use derive::derive;
pub use derive::derive_with;
#[cfg(test)]
pub(crate) use derive::test_default;

#[allow(unused_imports)] // see above
pub use report::{
    describe_snapshot, read_snapshot, write_snapshot, Override, OverrideReason, ResourceReport,
};

/// Every limit svcd derives. See [`derive_with`].
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
    pub log_level: crate::platform::log::Level,
    /// Master switch: `false` suppresses every log line (file and stderr).
    pub log_enabled: bool,
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
    /// Jobs the blocking pool may queue before it answers 503 busy.
    pub blocking_queue: usize,
    /// Share of the machine's CPU the daemon may use (0 = no cap). Read by the
    /// admission governor; the memory invariant does not spend it.
    pub cpu_budget_percent: u8,
}

/// The derived value of everything, plus the report that explains it.
#[derive(Debug, Clone)]
pub struct Derived {
    pub limits: Limits,
    pub report: ResourceReport,
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

/// The one bounded UI event ring behind every plugin stream. Charged once:
/// subscribers hold a cursor instead of a queue, so this does not grow with
/// subscriber or plugin count.
pub const UI_RING_BYTES: u64 = 2 * 1024 * 1024;
/// Resident cost of one subscriber's cursor in the ring's list.
pub const SUBSCRIBER_BYTES: u64 = 128;

/// The absolute ceiling on a derived connection count. It matches the spec's
/// upper bound; the invariant normally stops far below it.
pub const MAX_CONNECTIONS_CEILING: usize = 4096;

impl Limits {
    /// Threads the invariant charges a stack for.
    pub fn threads(&self) -> usize {
        threads_for(self.max_connections, self.blocking_threads)
    }

    /// What one admitted connection costs: its worst-case buffers, or the
    /// measured resident floor when the buffers are smaller than reality.
    pub fn per_connection_bytes(&self) -> u64 {
        (self.max_complete_body + self.stream_buffer as u64).max(CONNECTION_RESIDENT_BYTES)
    }

    /// How many connection slots (and streams, one per slot) the rest of this
    /// worst case leaves over.
    ///
    /// The connection count stops being a share of the budget and becomes what
    /// actually fits: every other term (baseline, thread stacks, stream
    /// buffers, in-flight bytes, queues, the UI ring) is already known, so the
    /// remainder divided by one slot's worst case is the answer.
    pub fn fitting_concurrency(&self, budget: u64) -> usize {
        let per = self
            .per_connection_bytes()
            .saturating_add(derive::stack_per_connection(self))
            .max(1);
        // One slot carries one stream and one subscriber cursor, so both are
        // part of the slot's cost when the three counts are solved together.
        let per_slot = per
            .saturating_add(self.stream_buffer as u64)
            .saturating_add(SUBSCRIBER_BYTES);
        let others = self
            .worst_case_bytes()
            .saturating_sub(self.max_connections as u64 * per)
            .saturating_sub(self.max_streams as u64 * self.stream_buffer as u64);
        let fits = budget.saturating_sub(others) / per_slot;
        (fits as usize).clamp(1, MAX_CONNECTIONS_CEILING)
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
        let connections = self.max_connections as u64 * self.per_connection_bytes();
        let streams = self.max_streams as u64 * self.stream_buffer as u64;
        let inflight = self.ws_inflight_bytes as u64;
        let queues = (self.pending_queue as u64 + self.progress_queue as u64) * IPC_FRAME_MAX
            + self.ws_raw_queue as u64 * self.ws_raw_max_bytes as u64;
        let ui = UI_RING_BYTES + self.max_subscriptions as u64 * SUBSCRIBER_BYTES;
        RESIDENT_BASELINE_BYTES
            .saturating_add(threads)
            .saturating_add(connections)
            .saturating_add(streams)
            .saturating_add(inflight)
            .saturating_add(queues)
            .saturating_add(ui)
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
                "connections (buffers or measured floor)",
                self.max_connections as u64 * self.per_connection_bytes(),
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
            (
                "ui event ring",
                UI_RING_BYTES + self.max_subscriptions as u64 * SUBSCRIBER_BYTES,
            ),
        ]
    }
}

#[cfg(test)]
mod tests;
