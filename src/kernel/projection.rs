//! 值投影：面中立的值形状（ADR-0025）。
//!
//! 一个服务对外可见的状态由两半组成：定义（[`crate::kernel::registry::Service`]）
//! 与运行面（[`Live`]）。这里只做纯变换——不碰进程、socket、文件，也不认识任何
//! 协议；各对外面把这份值渲染成自己的报文（见 ADR-0025）。
//!
//! 运行面的三个时刻保持监督侧的**单调毫秒**，由 [`Status::live`] 用 [`Now`]
//! 锚定成 epoch 秒：跨层边界只传值，不传时钟。

use serde::{Deserialize, Serialize};

use crate::kernel::registry::{Kind, Restart, Service, State};
use crate::platform::clock::Now;

/// 一个服务此刻由监督决定的运行事实。
///
/// 只收状态投影真正读到的字段；`Machine` 的 `remain_after_exit` /
/// `fail_after_stop` / `degraded` 不进——今天没有任何对外面读到它们。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Live {
    pub state: State,
    pub pid: Option<u32>,
    pub failures: u32,
    /// 单调毫秒。
    pub since_ms: u64,
    /// 单调毫秒。
    pub ready_since_ms: Option<u64>,
    /// 单调毫秒。
    pub next_at_ms: Option<u64>,
    pub last_exit: Option<i32>,
    pub unkillable: bool,
    pub restart: Restart,
}

/// The fixed `svc status` payload. `svc list` and the text output are both
/// projections of this shape; the control channel, HTTP face and MCP face all
/// consume the same JSON.
///
/// `since`, `ready_since` and `next_at` are Unix epoch seconds; `last_exit` is
/// the last recorded exit code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub summary: Summary,
    pub details: Details,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub kind: Kind,
    pub state: State,
    pub enabled: bool,
    pub pid: Option<u32>,
    pub since: Option<u64>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Details {
    pub dependencies: Vec<String>,
    pub last_exit: Option<i32>,
    pub next_at: Option<u64>,
    pub ready_since: Option<u64>,
    pub failures: u32,
    pub config: Option<String>,
    /// Crash-restart policy; only a live service carries it.
    pub restart: Option<String>,
    /// `true` only when a forced stop ran out of its kill window.
    pub unkillable: bool,
    /// Supplementary detail from the optional core status action; only
    /// `svc status` probes it, and its exit code never changes the service
    /// state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
}

impl Status {
    /// The on-disk/offline projection of a service definition.
    pub fn of(service: &Service) -> Self {
        Status {
            summary: Summary {
                id: service.id.clone(),
                kind: service.kind,
                state: service.state,
                enabled: service.enabled,
                pid: service.pid,
                since: service.since,
                version: service.version.clone(),
            },
            details: Details {
                dependencies: service.deps.clone(),
                last_exit: service.last_exit,
                next_at: service.next_at,
                ready_since: service.ready_since,
                failures: service.failures,
                config: service.config.clone(),
                restart: None,
                unkillable: false,
                probe: None,
            },
        }
    }

    /// The live projection of a supervised service. The [`Live`] facet carries
    /// monotonic stamps; the `now` pair anchors them back to epoch seconds.
    pub fn live(service: &Service, live: &Live, now: Now) -> Self {
        let mut status = Status::of(service);
        status.summary.state = live.state;
        status.summary.pid = live.pid;
        status.summary.since = anchored_secs(now, live.since_ms);
        status.details.last_exit = live.last_exit;
        status.details.next_at = anchored_secs_opt(now, live.next_at_ms);
        status.details.ready_since = anchored_secs_opt(now, live.ready_since_ms);
        status.details.failures = live.failures;
        status.details.restart = Some(live.restart.as_str().into());
        status.details.unkillable = live.unkillable;
        status
    }

    /// The in-process `svc` service is always running and never supervised.
    pub fn builtin(service: &Service) -> Self {
        let mut status = Status::of(service);
        status.summary.state = State::Running;
        status.summary.enabled = true;
        status.summary.pid = None;
        status.summary.since = None;
        status.details.restart = Some(service.restart.as_str().into());
        status
    }
}

/// Anchor a monotonic supervisor stamp to epoch seconds; `0` means "never
/// happened", so it stays [`None`] instead of becoming a plausible-looking but
/// wrong epoch second.
///
/// This is the production's only implementation of that conversion. Both
/// consumers call it: [`Status::live`] for the status projection and
/// `daemon::persist_states` for the runtime record. Keeping the guard here is
/// what makes the same fact (`one start`) produce the same `since` in
/// `svc status` and `<data>/run/<id>.json`.
pub(crate) fn anchored_secs(now: Now, ms: u64) -> Option<u64> {
    if ms == 0 {
        return None;
    }
    Some(now.epoch_of(ms))
}

pub(crate) fn anchored_secs_opt(now: Now, ms: Option<u64>) -> Option<u64> {
    ms.and_then(|value| anchored_secs(now, value))
}

/// The inverse of [`anchored_secs`]: epoch seconds back to the supervisor's
/// monotonic timeline.
///
/// A missing value stays missing. A runtime record without `since` means
/// "never happened"; substituting `now` would invent a start that the status
/// projection then reports as a plausible-looking epoch second.
pub(crate) fn anchored_mono_ms(now: Now, epoch: Option<u64>) -> Option<u64> {
    epoch.map(|value| now.mono_of(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::fixtures::test_service;

    #[test]
    fn json_has_the_fixed_summary_and_details_shape() {
        let value = serde_json::to_value(Status::of(&test_service())).expect("serialize");

        assert_eq!(
            value,
            serde_json::json!({
                "summary": {
                    "id": "demo",
                    "kind": "core",
                    "state": "running",
                    "enabled": true,
                    "pid": 42,
                    "since": 1_700_000_000u64,
                    "version": "1.2.3"
                },
                "details": {
                    "dependencies": ["base"],
                    "last_exit": 3,
                    "next_at": null,
                    "ready_since": 1_700_000_002u64,
                    "failures": 0,
                    "config": "/var/lib/svcd/demo/config",
                    "restart": null,
                    "unkillable": false
                }
            })
        );

        let object = value.as_object().expect("object");
        assert_eq!(object.len(), 2);
        assert!(object.contains_key("summary"));
        assert!(object.contains_key("details"));
    }

    #[test]
    fn stopped_service_serializes_null_runtime_fields() {
        let mut service = test_service();
        service.state = State::Stopped;
        service.enabled = false;
        service.pid = None;
        service.since = None;
        service.ready_since = None;
        service.last_exit = None;
        service.config = None;

        let value = serde_json::to_value(Status::of(&service)).expect("serialize");

        assert_eq!(value["summary"]["state"], "stopped");
        assert_eq!(value["summary"]["enabled"], false);
        assert!(value["summary"]["pid"].is_null());
        assert!(value["summary"]["since"].is_null());
        assert!(value["details"]["last_exit"].is_null());
        assert!(value["details"]["config"].is_null());
        assert!(value["details"]["restart"].is_null());
        assert_eq!(value["details"]["unkillable"], false);
    }

    fn live() -> Live {
        Live {
            state: State::Backoff,
            pid: None,
            failures: 2,
            since_ms: 1_700_000_000_000,
            ready_since_ms: None,
            next_at_ms: Some(1_700_000_005_000),
            last_exit: Some(1),
            unkillable: true,
            restart: Restart::OnFailure,
        }
    }

    #[test]
    fn live_status_anchors_the_facet_to_epoch_seconds() {
        let status = Status::live(
            &test_service(),
            &live(),
            Now::at(1_700_000_010_000, 1_700_000_010),
        );

        assert_eq!(status.summary.state, State::Backoff);
        assert_eq!(status.summary.pid, None);
        assert_eq!(status.summary.since, Some(1_700_000_000));
        assert_eq!(status.details.next_at, Some(1_700_000_005));
        assert_eq!(status.details.ready_since, None);
        assert_eq!(status.details.failures, 2);
        assert_eq!(status.details.last_exit, Some(1));
        assert_eq!(status.details.restart.as_deref(), Some("on-failure"));
        assert!(status.details.unkillable);
    }

    #[test]
    fn a_never_started_facet_reports_no_since() {
        let mut live = live();
        live.since_ms = 0;

        let status = Status::live(
            &test_service(),
            &live,
            Now::at(1_700_000_010_000, 1_700_000_010),
        );

        assert_eq!(status.summary.since, None);
    }

    #[test]
    fn builtin_is_running_and_never_has_a_pid() {
        let status = Status::builtin(&test_service());

        assert_eq!(status.summary.state, State::Running);
        assert!(status.summary.enabled);
        assert_eq!(status.summary.pid, None);
        assert_eq!(status.summary.since, None);
        assert_eq!(status.details.restart.as_deref(), Some("on-failure"));
    }
}
