//! Durable per-service runtime records.
//!
//! One compact JSON file per supervised service under `<data>/run/<id>.json`.
//! The record is the hand-off between two daemon generations: the next daemon
//! validates `boot` + `pid` + `starttime` before adopting a process, and a
//! record from another boot is discarded instead of trusted.
//!
//! All timestamps are Unix epoch seconds. The supervisor itself runs on
//! monotonic milliseconds, so `daemon.rs` converts both ways.

use std::fs;
use std::path::PathBuf;

#[cfg(test)]
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::paths;
use crate::registry::State;

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

#[cfg(test)]
static TEST_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);
#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Test-only override. Production resolves the run directory from paths;
/// tests need two different roots without racing on a process-global env var.
fn run_dir() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(root) = TEST_ROOT
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            return root.clone();
        }
    }
    paths::run_dir()
}

pub fn path(id: &str) -> PathBuf {
    run_dir().join(format!("{}.json", id))
}

pub fn load(id: &str) -> Option<Record> {
    let path = path(id);
    let bytes = fs::read(&path).ok()?;
    match serde_json::from_slice::<Record>(&bytes) {
        Ok(record) => Some(record),
        Err(error) => {
            crate::log::svcd(&format!(
                "svcd: {}: unreadable runtime record {}: {}",
                id,
                path.display(),
                error
            ));
            None
        }
    }
}

/// Write the record atomically (`<id>.json.tmp` + rename). A crash mid-write
/// leaves the previous record intact.
pub fn save(id: &str, record: &Record) -> Result<(), String> {
    let directory = run_dir();
    fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {}", directory.display(), error))?;
    let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    let temporary = directory.join(format!("{}.json.tmp", id));
    let target = path(id);
    fs::write(&temporary, bytes)
        .map_err(|error| format!("write {}: {}", temporary.display(), error))?;
    fs::rename(&temporary, &target)
        .map_err(|error| format!("rename {}: {}", target.display(), error))
}

pub fn remove(id: &str) {
    let target = path(id);
    match fs::remove_file(&target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            crate::log::svcd(&format!(
                "svcd: {}: cannot remove {}: {}",
                id,
                target.display(),
                error
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let base = crate::test_support::temp_dir();
        base.join(format!("svcd-runtime-{}-{}", std::process::id(), nanos))
    }

    fn with_root<T>(body: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_root();
        *TEST_ROOT
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(root.join("run"));
        let result = body(&root);
        *TEST_ROOT
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let _ = fs::remove_dir_all(&root);
        result
    }

    #[test]
    fn a_record_round_trips_through_the_file() {
        with_root(|root| {
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
            save("demo", &record).expect("save");
            assert_eq!(load("demo"), Some(record));
            let leftovers: Vec<String> = fs::read_dir(root.join("run"))
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
            remove("demo");
            assert_eq!(load("demo"), None);
        });
    }

    #[test]
    fn a_corrupt_record_is_ignored_instead_of_crashing() {
        with_root(|_root| {
            fs::create_dir_all(run_dir()).expect("run dir");
            fs::write(path("demo"), b"{not json").expect("corrupt record");
            assert_eq!(load("demo"), None);
            remove("demo");
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
}
