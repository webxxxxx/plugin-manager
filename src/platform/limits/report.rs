//! The resource report and the startup snapshot it renders.
//!
//! Every rewrite the derivation performs is recorded here, so the operator sees
//! what changed even when they never named the key; the same report is what
//! `svc status --resources` reads back.

use std::fmt;

use crate::platform::resources::{Budget, ResourceProfile, Workload, WorkloadSource};

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

/// Everything the composition root derives along with the [`Limits`](super::Limits): what the
/// machine reported, how much the process may plan for, how it spends it, and
/// every rewrite the derivation performed.
#[derive(Debug, Clone)]
pub struct ResourceReport {
    pub profile: ResourceProfile,
    pub budget: Budget,
    pub workload: Workload,
    pub workload_source: WorkloadSource,
    /// [`BUDGET_CAP`](super::BUDGET_CAP) for this build.
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
