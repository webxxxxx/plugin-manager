//! Where each installed plugin actually lives: `<data_dir>/plugin-locations.json`.
//!
//! Plugin roots are a list, not one directory (ADR-0030). A plugin may sit on
//! any of them, so "which root holds plugin X" is a recorded fact, not a
//! guess from the list order: deleting, exporting or upgrading X has to use
//! the record, and the record has to survive the root disappearing from the
//! configuration.
//!
//! The disk stays the truth. The file is a hint that a scan rebuilds whenever
//! it disagrees with what is on the roots, with an atomic replace so a crash
//! mid-write cannot truncate it. A missing or unreadable file is not an error:
//! every lookup falls back to the default root, which is exactly the
//! single-root layout of ADR-0023.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::platform::paths::Paths;

const FILE: &str = "plugin-locations.json";
const VERSION: u64 = 1;

/// One installed plugin directory found on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub id: String,
    pub root: PathBuf,
}

/// What a scan over every root saw.
#[derive(Clone, Debug, Default)]
pub(crate) struct Discovery {
    /// One entry per id, in root order; the first root that has it wins.
    pub found: Vec<Found>,
    /// Ids that exist in more than one root. Nothing is deleted: the message
    /// names both paths so an operator can decide.
    pub conflicts: Vec<String>,
    /// Directories a root could not be read from.
    pub warnings: Vec<String>,
}

impl Discovery {
    /// The map written to the index: id -> root.
    pub fn map(&self) -> BTreeMap<String, PathBuf> {
        self.found
            .iter()
            .map(|found| (found.id.clone(), found.root.clone()))
            .collect()
    }
}

pub(crate) fn index_path(paths: &Paths) -> PathBuf {
    paths.data_dir.join(FILE)
}

/// The whole index as recorded.
pub(crate) fn load(paths: &Paths) -> BTreeMap<String, PathBuf> {
    read(&index_path(paths)).0
}

/// The recorded root of one plugin, if the index knows it.
pub(crate) fn root_of(paths: &Paths, id: &str) -> Option<PathBuf> {
    load(paths).remove(id)
}

/// The directory of one installed plugin. Falls back to the default root so a
/// missing index behaves like the single-root layout.
pub(crate) fn dir_of(paths: &Paths, id: &str) -> PathBuf {
    root_of(paths, id)
        .unwrap_or_else(|| paths.default_plugin_root().to_path_buf())
        .join(id)
}

/// Free bytes on the volume that holds `path`. The directory may not exist yet
/// (a root that has never been used), so walk up to the nearest ancestor: the
/// filesystem answers for the mount, not for the leaf.
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    use crate::platform::resources::{Host, Machine};

    let mut candidate = Some(path);
    while let Some(directory) = candidate {
        if directory.is_dir() {
            return Host.free_disk(directory);
        }
        candidate = directory.parent();
    }
    None
}

/// Every plugin id under one root: a directory with a `current` pointer. The
/// pointer is what makes it an installed plugin rather than a leftover
/// directory, a staging tree or the operator's notes.
pub(crate) fn installed_in(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .filter(|entry| entry.path().join("current").is_file())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|id| !id.starts_with('.'))
        .collect();
    ids.sort();
    ids
}

/// Read the index, then reconcile it with the roots: the disk wins.
///
/// Returns the map the index should hold, whether it differs from the file,
/// and the warnings a caller should log. Every root is listed in priority
/// order; an id in two roots is kept at its first root and reported.
pub(crate) fn discover(roots: &[PathBuf]) -> Discovery {
    let mut discovery = Discovery::default();
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
    for root in roots {
        if !root.is_dir() {
            // A root may live on a volume that is not mounted right now. Its
            // plugins keep their index entries; nothing here deletes them.
            if root.exists() {
                discovery
                    .warnings
                    .push(format!("plugin root {} is not a directory", root.display()));
            }
            continue;
        }
        for id in installed_in(root) {
            if let Some(first) = seen.get(&id) {
                if first != root {
                    discovery.conflicts.push(format!(
                        "plugin {} is installed in both {} and {}; using {}",
                        id,
                        first.display(),
                        root.display(),
                        first.display()
                    ));
                }
                continue;
            }
            seen.insert(id.clone(), root.clone());
            discovery.found.push(Found {
                id,
                root: root.clone(),
            });
        }
    }
    discovery
}

/// Write the whole index atomically: a crash leaves either the old file or
/// the new one, never a truncated one. `id -> root` is stored as absolute
/// paths (the resolver always produces them).
pub(crate) fn save(paths: &Paths, map: &BTreeMap<String, PathBuf>) -> Result<(), String> {
    let path = index_path(paths);
    let plugins: Value = map
        .iter()
        .map(|(id, root)| (id.clone(), json!(root.to_string_lossy())))
        .collect::<serde_json::Map<String, Value>>()
        .into();
    let body = json!({ "version": VERSION, "plugins": plugins });
    let body = serde_json::to_vec_pretty(&body).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {}", parent.display(), error))?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, &body)
        .map_err(|error| format!("write {}: {}", temporary.display(), error))?;
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("replace {}: {}", path.display(), error)
    })
}

/// Record one plugin's root without losing the other entries. A read failure
/// rebuilds nothing: the entry is added to whatever could be read.
pub(crate) fn record(paths: &Paths, id: &str, root: &Path) -> Result<(), String> {
    let mut map = read(&index_path(paths)).0;
    map.insert(id.to_string(), root.to_path_buf());
    save(paths, &map)
}

/// Parse the file. A broken document is treated as absent: the scan rebuilds
/// it from the roots, and a daemon that refuses to boot because a cache file
/// is corrupt would be worse than the stale cache.
fn read(path: &Path) -> (BTreeMap<String, PathBuf>, Vec<String>) {
    let mut map = BTreeMap::new();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (map, Vec::new()),
        Err(error) => {
            return (
                map,
                vec![format!("cannot read {}: {}", path.display(), error)],
            )
        }
    };
    let document: Value = match serde_json::from_str(&text) {
        Ok(document) => document,
        Err(error) => {
            return (
                map,
                vec![format!(
                    "{} is not valid JSON: {}; rebuilding",
                    path.display(),
                    error
                )],
            )
        }
    };
    let version = document.get("version").and_then(Value::as_u64);
    if version != Some(VERSION) {
        return (
            map,
            vec![format!(
                "{} has version {:?}, expected {}; rebuilding",
                path.display(),
                version,
                VERSION
            )],
        );
    }
    if let Some(plugins) = document.get("plugins").and_then(Value::as_object) {
        for (id, root) in plugins {
            if let Some(root) = root.as_str() {
                map.insert(id.clone(), PathBuf::from(root));
            }
        }
    }
    (map, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_support::TempDir;

    fn plugin_dir(root: &Path, id: &str) -> PathBuf {
        let directory = root.join(id);
        fs::create_dir_all(directory.join("1.0.0")).expect("version");
        fs::write(directory.join("current"), "1.0.0").expect("current");
        directory
    }

    fn paths_with(root: &Path, roots: Vec<PathBuf>) -> Paths {
        let mut paths = Paths::under(root);
        paths.plugin_roots = roots;
        // `Paths::under` derives these names; the tree itself is the caller's.
        fs::create_dir_all(&paths.data_dir).expect("data dir");
        paths
    }

    #[test]
    fn discovery_walks_every_root_and_keeps_root_order() {
        let root = TempDir::new();
        let first = root.0.join("internal/plugins");
        let second = root.0.join("sd/plugins");
        plugin_dir(&first, "alpha");
        plugin_dir(&second, "beta");

        let discovery = discover(&[first.clone(), second.clone()]);
        assert_eq!(discovery.conflicts, Vec::<String>::new());
        assert_eq!(
            discovery.found,
            vec![
                Found {
                    id: "alpha".into(),
                    root: first
                },
                Found {
                    id: "beta".into(),
                    root: second
                },
            ]
        );
        assert_eq!(discovery.map().len(), 2);
    }

    #[test]
    fn an_id_in_two_roots_is_a_conflict_and_the_first_root_wins() {
        let root = TempDir::new();
        let first = root.0.join("internal/plugins");
        let second = root.0.join("sd/plugins");
        plugin_dir(&first, "alpha");
        plugin_dir(&second, "alpha");

        let discovery = discover(&[first.clone(), second.clone()]);
        assert_eq!(discovery.found.len(), 1);
        assert_eq!(discovery.found[0].root, first);
        let conflict = discovery.conflicts.first().expect("conflict");
        assert!(conflict.contains("alpha"), "{conflict}");
        assert!(
            conflict.contains(&first.display().to_string())
                && conflict.contains(&second.display().to_string()),
            "the conflict must name both roots: {conflict}"
        );
    }

    #[test]
    fn a_directory_without_a_current_pointer_is_not_an_installed_plugin() {
        let root = TempDir::new();
        let plugins = root.0.join("plugins");
        fs::create_dir_all(plugins.join("notes")).expect("dir");
        plugin_dir(&plugins, "alpha");
        assert_eq!(installed_in(&plugins), vec!["alpha".to_string()]);
    }

    #[test]
    fn the_index_round_trips_and_a_second_root_survives_a_record() {
        let root = TempDir::new();
        let paths = paths_with(&root.0, vec![root.0.join("a"), root.0.join("b")]);
        record(&paths, "alpha", &root.0.join("b")).expect("record");
        record(&paths, "beta", &root.0.join("a")).expect("record");

        assert_eq!(root_of(&paths, "alpha"), Some(root.0.join("b")));
        assert_eq!(dir_of(&paths, "alpha"), root.0.join("b/alpha"));
        assert_eq!(dir_of(&paths, "gamma"), root.0.join("a/gamma"));
        assert_eq!(root_of(&paths, "gamma"), None);
    }

    #[test]
    fn a_corrupt_index_reads_as_empty_instead_of_failing_the_process() {
        let root = TempDir::new();
        let paths = paths_with(&root.0, vec![root.0.join("a")]);
        fs::write(index_path(&paths), b"{not json").expect("write");
        assert_eq!(root_of(&paths, "alpha"), None);
        let (map, warnings) = read(&index_path(&paths));
        assert!(map.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    /// An index written by a newer daemon is not silently reinterpreted.
    #[test]
    fn a_future_index_version_is_rebuilt_rather_than_trusted() {
        let root = TempDir::new();
        let paths = paths_with(&root.0, vec![root.0.join("a")]);
        fs::write(
            index_path(&paths),
            br#"{"version": 99, "plugins": {"alpha": "/elsewhere"}}"#,
        )
        .expect("write");
        let (map, warnings) = read(&index_path(&paths));
        assert!(map.is_empty(), "{map:?}");
        assert!(warnings[0].contains("99"), "{warnings:?}");
    }

    #[test]
    fn a_root_that_is_not_mounted_is_a_warning_not_a_deleted_entry() {
        let root = TempDir::new();
        let missing = root.0.join("not-mounted/plugins");
        let discovery = discover(&[missing]);
        assert!(discovery.found.is_empty());
        assert_eq!(
            discovery.warnings,
            Vec::<String>::new(),
            "a missing root is not an error"
        );
    }
}
