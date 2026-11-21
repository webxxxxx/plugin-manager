//! Derivation: apply the key table to a document and a machine profile.
//!
//! Pure: the same document plus the same profile always yields the same limits
//! (user story 14); every rewrite is recorded as an `Override`.

use std::time::Duration;

use crate::platform::resources::{budget, ResourceProfile, Workload, WorkloadSource};
use crate::platform::settings;

use super::report::{Override, OverrideReason, ResourceReport};
use super::specs::*;
use super::{Derived, Limits, MAX_CONNECTIONS_CEILING};

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
    let budget = budget(
        settings.mem_budget_bytes,
        settings.mem_budget_percent,
        profile,
        BUDGET_CAP,
    );
    let workload = settings
        .workload
        .unwrap_or_else(|| Workload::suggested(budget.quantized, profile.cpus));
    let workload_source = match settings.workload {
        Some(_) => WorkloadSource::Configured,
        None => WorkloadSource::Derived,
    };

    let mut notes = Vec::new();
    // The ceiling still tightens an explicit number; the floor and the
    // quantization step apply to `auto` only, so there is no below-floor
    // rewrite of `mem_budget_bytes` to report here.
    if let Some(configured) = settings.mem_budget_bytes {
        if configured > BUDGET_CAP {
            notes.push(Override::new(
                "mem_budget_bytes",
                configured,
                BUDGET_CAP,
                OverrideReason::BuildCap,
            ));
        }
    }
    // A percentage is resolved against the probe before the ceiling can tighten
    // it, so the row keeps the key the operator wrote and shows the effective
    // budget in bytes on the right: `mem_budget_percent: 90 -> 2147483648`.
    if let Some(percent) = settings.mem_budget_percent {
        let requested = profile
            .available_memory
            .map(|available| available.saturating_mul(percent as u64) / 100)
            .unwrap_or(BUDGET_CAP);
        if requested > BUDGET_CAP {
            notes.push(Override::new(
                "mem_budget_percent",
                percent as u64,
                BUDGET_CAP,
                OverrideReason::BuildCap,
            ));
        }
    }

    let mut derivation = Derivation {
        workload,
        budget: budget.quantized,
        notes: &mut notes,
    };
    let mut limits = Limits::from_settings(SettingsView::from(settings), &mut derivation);
    solve_connections(&mut limits, budget.quantized, settings);
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
    blocking_queue: Option<usize>,
    cpu_budget_percent: Option<usize>,
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
            blocking_queue: settings.blocking_queue,
            cpu_budget_percent: settings.cpu_budget_percent,
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
        let blocking_queue =
            derivation.count(&BLOCKING_QUEUE, settings.blocking_queue.map(|v| v as u64));
        let cpu_budget_percent = derivation.count(
            &CPU_BUDGET_PERCENT,
            settings.cpu_budget_percent.map(|v| v as u64),
        ) as u8;

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
            blocking_queue,
            cpu_budget_percent,
        }
    }
}

/// When the document names no connection count, the budget does.
///
/// A connection slot used to be an arbitrary share of the budget (16 MiB/3 at
/// tiny, which stopped a 50 MiB budget at 9 connections while one connection's
/// worst case is 29 KiB). The slot is now what the rest of the worst case
/// leaves over, which is the same rule every other count follows. An explicit
/// number still wins, and the invariant still shrinks it when it does not fit.
///
/// The per-IP cap defaults to the same number: a per-IP cap below the global
/// one makes the global cap unreachable for every single-host deployment (the
/// console, the CLI and a reverse proxy all arrive from one address). The key
/// stays available for operators who want shares between many clients.
fn solve_connections(limits: &mut Limits, budget: u64, settings: &settings::Settings) {
    if settings.max_connections.is_none() {
        let slots = limits.fitting_concurrency(budget);
        limits.max_connections = slots;
        if settings.max_streams.is_none() {
            limits.max_streams = slots.min(STREAM_CEILING);
        }
        if settings.max_subscriptions.is_none() {
            limits.max_subscriptions = slots.min(SUBSCRIPTION_CEILING);
        }
    } else {
        // A stream lives on a connection and a UI subscription on a stream, so
        // an unconfigured cap above the connection count cannot be reached
        // anyway. Tying the three to one number keeps a small budget from
        // advertising capacity it can never hand out.
        if settings.max_streams.is_none() {
            limits.max_streams = limits.max_connections.min(STREAM_CEILING);
        }
        if settings.max_subscriptions.is_none() {
            limits.max_subscriptions = limits.max_connections.min(SUBSCRIPTION_CEILING);
        }
    }
    if settings.max_connections_per_ip.is_none() {
        limits.max_connections_per_ip = limits.max_connections;
    }
    // A queued blocking job belongs to an admitted connection, so a queue
    // larger than the connection count can never fill and only misleads.
    if settings.blocking_queue.is_none() {
        limits.blocking_queue = limits.max_connections.min(MAX_BLOCKING_QUEUE);
    } else {
        limits.blocking_queue = limits.blocking_queue.min(limits.max_connections);
    }
}

/// The largest queue the solver hands out when the document stays silent.
/// It matches the connection ceiling: a queued job belongs to an admitted
/// connection, so the queue can never be longer than that anyway. The old 256
/// was the wall at fifty workers per plugin.
const MAX_BLOCKING_QUEUE: usize = 4096;

/// The absolute ceiling for a solved stream count: the connection ceiling.
///
/// A stream lives on a connection, and the common realtime shape is one stream
/// per connection (a plugin UI WebSocket). While this sat at 1024, a budget
/// that solved to 1459 connections could still only hand out 1024 streams, so
/// three quarters of the connection capacity was unreachable for exactly the
/// workload that needs it most.
const STREAM_CEILING: usize = MAX_CONNECTIONS_CEILING;
/// The absolute ceiling for a solved subscription count.
const SUBSCRIPTION_CEILING: usize = 4096;

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
pub(super) fn stack_per_connection(limits: &Limits) -> u64 {
    if cfg!(windows) {
        // Charged again per connection by `threads_for`, so the solver must
        // account for it or it would stop one iteration too early.
        limits.request_stack_bytes as u64
    } else {
        0
    }
}
