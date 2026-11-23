//! Durable per-service runtime records.
//!
//! One compact JSON file per supervised service under `<data>/run/<id>.json`.
//! The record is the hand-off between two daemon generations: the next daemon
//! validates `boot` + `pid` + `starttime` before adopting a process, and a
//! record from another boot is discarded instead of trusted.
//!
//! All timestamps are Unix epoch seconds. The supervisor itself runs on
//! monotonic milliseconds, so `daemon.rs` converts both ways.
//!
//! The run directory is a construction parameter: this module never resolves a
//! process-wide directory, so a test can point it at its own temporary root.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::kernel::registry::State;
use crate::platform::paths::Paths;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// `/proc/sys/kernel/random/boot_id` of the boot that wrote the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<String>,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// `/proc/<pid>/stat` field 22, captured when the pid was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starttime: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_since: Option<u64>,
    #[serde(default)]
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<i32>,
}

fn path(id: &str, paths: &Paths) -> PathBuf {
    paths.run_dir.join(format!("{}.json", id))
}

pub fn load(id: &str, paths: &Paths) -> Option<Record> {
    let path = path(id, paths);
    let bytes = fs::read(&path).ok()?;
    match serde_json::from_slice::<Record>(&bytes) {
        Ok(record) => Some(record),
        Err(error) => {
            warn!(
                "{}: unreadable runtime record {}: {}",
                id,
                path.display(),
                error
            );
            None
        }
    }
}

/// Write the record atomically (`<id>.json.tmp` + rename). A crash mid-write
/// leaves the previous record intact.
pub fn save(id: &str, record: &Record, paths: &Paths) -> Result<(), String> {
    let directory = paths.run_dir.clone();
    fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {}", directory.display(), error))?;
    let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    let temporary = directory.join(format!("{}.json.tmp", id));
    let target = path(id, paths);
    fs::write(&temporary, bytes)
        .map_err(|error| format!("write {}: {}", temporary.display(), error))?;
    fs::rename(&temporary, &target)
        .map_err(|error| format!("rename {}: {}", target.display(), error))
}

pub fn remove(id: &str, paths: &Paths) {
    let target = path(id, paths);
    match fs::remove_file(&target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            warn!("{}: cannot remove {}: {}", id, target.display(), error);
        }
    }
}

/// True when the record belongs to the current boot (or the platform cannot
/// report a boot id, in which case pid+starttime is the only guard).
pub fn same_boot(record: &Record, boot: Option<&str>) -> bool {
    match (record.boot.as_deref(), boot) {
        (Some(recorded), Some(current)) => recorded == current,
        _ => true,
    }
}

/// What the platform host can say about the pid a record names.
///
/// The host probes `alive` / `start_time`; this model never touches the OS. A
/// dead pid carries no start time, so `Gone` cannot carry one either: the
/// invariant lives in the type instead of in each caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStamp {
    Gone,
    Alive { starttime: Option<u64> },
}

/// What the record's pid refers to now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PidState {
    /// No live process holds the pid.
    Gone,
    /// The live process has the recorded start time.
    Same,
    /// A live process holds the pid with a different start time: the pid was
    /// reused, so it is not the process the record was written for.
    Reused { expected: u64, actual: u64 },
    /// A live process holds the pid, but one side has no start time: reuse
    /// cannot be proved, so the record is trusted. This is the boundary the
    /// platform has always used, and it is not proof of identity.
    Unknown,
}

/// Classify the record's pid from one platform stamp.
///
/// This is the only implementation of the pid + starttime rule: adoption and
/// cleanup both consume [`PidState`], so the "reused" and "cannot prove reuse"
/// cases cannot drift apart.
pub fn pid_state(record: &Record, stamp: ProcessStamp) -> PidState {
    let ProcessStamp::Alive { starttime } = stamp else {
        return PidState::Gone;
    };
    match (record.starttime, starttime) {
        (Some(expected), Some(actual)) if expected != actual => {
            PidState::Reused { expected, actual }
        }
        (Some(_), Some(_)) => PidState::Same,
        _ => PidState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let base = crate::platform::test_support::temp_dir();
        base.join(format!("svcd-runtime-{}-{}", std::process::id(), nanos))
    }

    /// The temporary root owns the run directory for exactly one test; two
    /// tests cannot race through process state.
    fn with_root<T>(body: impl FnOnce(&std::path::Path, &Paths) -> T) -> T {
        let root = temp_root();
        let paths = Paths::under(&root);
        let result = body(&root, &paths);
        let _ = fs::remove_dir_all(&root);
        result
    }

    #[test]
    fn a_record_round_trips_through_the_file() {
        with_root(|_root, paths| {
            let record = Record {
                boot: Some("boot-1".into()),
                state: State::Running,
                pid: Some(42),
                starttime: Some(1234),
                since: Some(1_700_000_000),
                ready_since: Some(1_700_000_001),
                failures: 2,
                last_exit: Some(1),
            };
            save("demo", &record, paths).expect("save");
            assert_eq!(load("demo", paths), Some(record));
            let leftovers: Vec<String> = fs::read_dir(&paths.run_dir)
                .expect("run dir")
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .filter(|name| name.contains(".tmp"))
                .collect();
            assert!(
                leftovers.is_empty(),
                "temporary files left: {:?}",
                leftovers
            );
            remove("demo", paths);
            assert_eq!(load("demo", paths), None);
        });
    }

    #[test]
    fn a_corrupt_record_is_ignored_instead_of_crashing() {
        with_root(|_root, paths| {
            fs::create_dir_all(&paths.run_dir).expect("run dir");
            fs::write(path("demo", paths), b"{not json").expect("corrupt record");
            assert_eq!(load("demo", paths), None);
            remove("demo", paths);
        });
    }

    #[test]
    fn a_record_from_another_boot_is_not_trusted() {
        let record = Record {
            boot: Some("boot-1".into()),
            state: State::Running,
            pid: Some(42),
            starttime: None,
            since: None,
            ready_since: None,
            failures: 0,
            last_exit: None,
        };
        assert!(same_boot(&record, Some("boot-1")));
        assert!(!same_boot(&record, Some("boot-2")));
        assert!(same_boot(&record, None), "no boot id means no check");
        let legacy = Record {
            boot: None,
            ..record
        };
        assert!(same_boot(&legacy, Some("boot-2")));
    }

    #[test]
    fn a_pid_is_only_trusted_when_reuse_cannot_be_proved() {
        let record = Record {
            boot: Some("boot-1".into()),
            state: State::Running,
            pid: Some(42),
            starttime: Some(7_000),
            since: None,
            ready_since: None,
            failures: 0,
            last_exit: None,
        };

        assert_eq!(pid_state(&record, ProcessStamp::Gone), PidState::Gone);
        assert_eq!(
            pid_state(
                &record,
                ProcessStamp::Alive {
                    starttime: Some(7_000)
                }
            ),
            PidState::Same
        );
        assert_eq!(
            pid_state(
                &record,
                ProcessStamp::Alive {
                    starttime: Some(8_000)
                }
            ),
            PidState::Reused {
                expected: 7_000,
                actual: 8_000
            },
            "a mismatching start time means the pid was reused"
        );
        assert_eq!(
            pid_state(&record, ProcessStamp::Alive { starttime: None }),
            PidState::Unknown,
            "a live pid without a current start time cannot prove reuse"
        );
        let legacy = Record {
            starttime: None,
            ..record
        };
        assert_eq!(
            pid_state(
                &legacy,
                ProcessStamp::Alive {
                    starttime: Some(8_000)
                }
            ),
            PidState::Unknown,
            "a record without starttime cannot prove reuse"
        );
    }
}
