//! Which plugin root an install lands on (ADR-0030).
//!
//! The rule is one place, because three callers need the same answer: the
//! streamed upload (which chooses before the first payload byte), the
//! path install behind `plugin.install`, and the CLI. "The smallest volume
//! that fits" keeps the big volume free for the next, bigger package; an
//! installed plugin never moves, so an upgrade stays where the index says.

use super::*;

/// One candidate root with what its volume reports. `None` is a volume the
/// platform could not measure (not mounted, permission denied): it may still
/// be usable, it just cannot win the "smallest that fits" comparison.
pub(crate) type Candidate = (PathBuf, Option<u64>);

/// The root an install writes to.
///
/// - An installed plugin stays on its recorded root. Asking for a different
///   one is refused, not silently honored: two copies of one id is the state
///   this whole index exists to avoid.
/// - An explicit root must be one of `plugins_dir`, otherwise it is a typo
///   dressed as a path.
/// - Otherwise the smallest volume that fits wins. Nothing fitting is a
///   refusal that names every root and its free bytes, so the operator can
///   see which volume is short.
pub(crate) fn install_root(
    paths: &Paths,
    id: &str,
    requested: Option<&Path>,
    needed: u64,
) -> Result<PathBuf, ProblemError> {
    // A root that is not configured is a malformed request whatever else is
    // true, so it is checked first: answering "already installed elsewhere"
    // would hide the typo.
    let requested = match requested {
        Some(requested) => Some(configured(paths, requested)?),
        None => None,
    };

    // One lookup for the refusal payload: how much an uninstall would return.
    // It is 0 when nothing is installed, which is the whole answer for a fresh
    // install.
    let installed = crate::manage::installed_version_bytes(paths, id);

    if let Some(recorded) = crate::platform::locations::root_of(paths, id) {
        if let Some(requested) = &requested {
            if normalize(requested) != normalize(&recorded) {
                return Err(Problem::Conflict.error(format!(
                    "plugin {} is installed in {}; installing it into {} would leave two copies — uninstall it first",
                    id,
                    recorded.display(),
                    requested.display()
                )));
            }
        }
        // Only the bytes this install writes are asked for: the old copy is
        // still on the volume until the commit, but it is already outside
        // `free`, so counting it again would refuse upgrades that fit (#329).
        // A root that cannot hold `needed` is refused here with numbers
        // instead of failing halfway through a staging tree.
        require_room(&recorded, id, needed, installed)?;
        return Ok(recorded);
    }

    if let Some(requested) = requested {
        require_room(&requested, id, needed, installed)?;
        return Ok(requested);
    }

    let candidates = candidates(paths);
    choose_root(&candidates, needed)
        .ok_or_else(|| insufficient(&candidates, needed, id, installed, None))
}

/// `root` needs `needed` bytes of room, or the install is refused with the
/// numbers. A root the platform cannot measure is left to fail on its own
/// write: refusing here would block a root that is simply not readable yet.
fn require_room(root: &Path, id: &str, needed: u64, installed: u64) -> Result<(), ProblemError> {
    require_room_at(
        root,
        id,
        needed,
        installed,
        crate::platform::locations::free_bytes,
    )
}

/// The check with the measurement injected, so the refusal is testable without
/// filling a real volume.
fn require_room_at(
    root: &Path,
    id: &str,
    needed: u64,
    installed: u64,
    free: impl Fn(&Path) -> Option<u64>,
) -> Result<(), ProblemError> {
    let Some(free) = free(root) else {
        return Ok(());
    };
    if free < needed {
        return Err(insufficient(
            &[(root.to_path_buf(), Some(free))],
            needed,
            id,
            installed,
            Some(root),
        ));
    }
    Ok(())
}

/// The configured spelling of `requested`, so the caller's `--root` and the
/// recorded root compare as the same string.
fn configured(paths: &Paths, requested: &Path) -> Result<PathBuf, ProblemError> {
    paths
        .plugin_roots
        .iter()
        .find(|root| normalize(root) == normalize(requested))
        .cloned()
        .ok_or_else(|| {
            Problem::Invalid.error(format!(
                "root {} is not one of plugins_dir ({})",
                requested.display(),
                list(&paths.plugin_roots)
            ))
        })
}

/// Every root with its free bytes, in priority order.
///
/// The default root is created here: it lives under `data_dir`, the host owns
/// it, and a first install needs something to measure. A configured extra root
/// is left alone — if its volume is not mounted, it must stay unmeasurable.
pub(crate) fn candidates(paths: &Paths) -> Vec<Candidate> {
    let _ = fs::create_dir_all(paths.default_plugin_root());
    paths
        .plugin_roots
        .iter()
        .map(|root| (root.clone(), crate::platform::locations::free_bytes(root)))
        .collect()
}

/// The smallest measured volume that can take `needed` bytes. Unmeasured roots
/// never win: they are unknown, not empty, and a wrong guess costs a failed
/// install halfway through the payload.
pub(crate) fn choose_root(candidates: &[Candidate], needed: u64) -> Option<PathBuf> {
    let mut measured: Vec<&Candidate> = candidates
        .iter()
        .filter(|(_, free)| free.is_some())
        .collect();
    measured.sort_by_key(|(_, free)| *free);
    measured
        .into_iter()
        .find(|(_, free)| free.unwrap_or(0) >= needed)
        .map(|(root, _)| root.clone())
}

/// The structured space refusal: its own category (HTTP 507) so a caller can
/// offer "uninstall the old version and retry" instead of showing a server
/// error, and the message carries the numbers.
///
/// The same numbers also go into `data`, because a caller cannot drive a
/// dialog off a sentence: `id` says which plugin to uninstall, `installed`
/// how much that returns, `need` what the check compared against, `root` the
/// root the check ran on (the only volume an uninstall frees), and `roots`
/// what each candidate had free.
fn insufficient(
    candidates: &[Candidate],
    needed: u64,
    id: &str,
    installed: u64,
    recorded: Option<&Path>,
) -> ProblemError {
    let rows: Vec<String> = candidates
        .iter()
        .map(|(root, free)| match free {
            Some(free) => format!("{} has {} bytes", root.display(), free),
            None => format!("{} is not available", root.display()),
        })
        .collect();
    let old = recorded
        .map(|root| {
            format!(
                "; the installed version in {} still takes room",
                root.display()
            )
        })
        .unwrap_or_default();
    let roots: Vec<serde_json::Value> = candidates
        .iter()
        .map(|(root, free)| {
            serde_json::json!({
                "root": root.display().to_string(),
                // A volume the platform could not measure stays `null`: the
                // caller must not read "unknown" as "empty".
                "free": free,
            })
        })
        .collect();
    Problem::InsufficientSpace
        .error(format!(
            "not enough space to install: need {} bytes, {}; free space on the volume or uninstall the old version first{}",
            needed,
            rows.join(", "),
            old
        ))
        .with_data(serde_json::json!({
            "id": id,
            "need": needed,
            "installed": installed,
            // Which volume an uninstall would free. `null` when the root was
            // not chosen yet: a fresh install has nothing to uninstall.
            "root": recorded.map(|root| root.display().to_string()),
            "roots": roots,
        }))
}

/// A lexical fallback for a root that does not exist yet: canonicalize answers
/// the same path twice only when both exist, and a configured root may not.
fn normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn list(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(root: &str, free: Option<u64>) -> Candidate {
        (PathBuf::from(root), free)
    }

    /// The smallest volume that fits wins, so a big volume stays available for
    /// the next, bigger package.
    #[test]
    fn the_smallest_volume_that_fits_wins() {
        let roots = [
            candidate("/data/plugins", Some(20_000_000)),
            candidate("/sd/plugins", Some(200_000_000)),
        ];
        assert_eq!(
            choose_root(&roots, 15_700_000),
            Some(PathBuf::from("/data/plugins"))
        );
        assert_eq!(
            choose_root(&roots, 30_000_000),
            Some(PathBuf::from("/sd/plugins"))
        );
    }

    /// Exactly enough is enough: the comparison is `>=`, and a one-byte
    /// shortfall moves to the next volume.
    #[test]
    fn an_exact_fit_is_a_fit() {
        let roots = [
            candidate("/small", Some(10)),
            candidate("/large", Some(100)),
        ];
        assert_eq!(choose_root(&roots, 10), Some(PathBuf::from("/small")));
        assert_eq!(choose_root(&roots, 11), Some(PathBuf::from("/large")));
    }

    #[test]
    fn nothing_fitting_or_nothing_measured_is_none() {
        let roots = [candidate("/tiny", Some(1)), candidate("/wat", None)];
        assert_eq!(choose_root(&roots, 2), None);
        assert_eq!(choose_root(&[], 0), None);
    }

    /// An unmeasured root is unknown, not empty: it must not win over a
    /// measured root that does fit.
    #[test]
    fn an_unmeasured_root_never_wins() {
        let roots = [candidate("/wat", None), candidate("/ok", Some(5))];
        assert_eq!(choose_root(&roots, 1), Some(PathBuf::from("/ok")));
    }

    /// Ties keep the configured order, so the default root stays the default
    /// when both roots are on the same volume (the usual dev/test case).
    #[test]
    fn a_tie_keeps_the_configured_order() {
        let roots = [
            candidate("/first", Some(100)),
            candidate("/second", Some(100)),
        ];
        assert_eq!(choose_root(&roots, 1), Some(PathBuf::from("/first")));
    }
    #[test]
    fn a_root_outside_the_list_is_a_bad_request() {
        let temp = crate::platform::test_support::TempDir::new();
        let paths =
            crate::platform::test_support::paths_with_roots(&temp.0, vec![temp.0.join("a")]);
        let error = install_root(&paths, "demo", Some(Path::new("/elsewhere")), 1)
            .expect_err("not configured");
        assert!(error.detail().contains("plugins_dir"), "{error}");
        assert!(error.detail().contains("/elsewhere"), "{error}");
    }

    #[test]
    fn an_installed_plugin_keeps_its_recorded_root_and_refuses_another_one() {
        let temp = crate::platform::test_support::TempDir::new();
        let first = temp.0.join("a");
        let second = temp.0.join("b");
        let paths = crate::platform::test_support::paths_with_roots(
            &temp.0,
            vec![first.clone(), second.clone()],
        );
        crate::platform::locations::record(&paths, "demo", &second).expect("record");

        assert_eq!(
            install_root(&paths, "demo", None, 1).expect("recorded"),
            second
        );
        assert_eq!(
            install_root(&paths, "demo", Some(&second), 1).expect("same root"),
            second
        );
        let error = install_root(&paths, "demo", Some(&first), 1).expect_err("two copies");
        assert_eq!(error.problem, Problem::Conflict);
        assert!(error.detail().contains("uninstall"), "{error}");
    }

    /// An upgrade stages the new copy while the old one still sits on the
    /// volume, so the check asks for the new tree alone: the installed bytes
    /// are already outside `free` (#329). When they do not fit, the answer is
    /// the structured space refusal, not a half-written staging tree.
    #[test]
    fn a_recorded_root_that_cannot_hold_the_new_tree_is_a_space_refusal() {
        let error = require_room_at(
            Path::new("/data/plugins"),
            "demo",
            31_400_000,
            12_000_000,
            |_| Some(17_500_000),
        )
        .expect_err("the new tree does not fit");
        assert_eq!(error.problem, Problem::InsufficientSpace);
        assert!(error.detail().contains("31400000"), "{error}");
        assert!(error.detail().contains("17500000"), "{error}");
        assert!(error.detail().contains("/data/plugins"), "{error}");

        // The payload is what a dialog runs on: which plugin to uninstall,
        // what the check needed, how much an uninstall would return, and the
        // root it would return it to.
        let data = error.data().expect("the refusal carries numbers");
        assert_eq!(data["id"], "demo");
        assert_eq!(data["need"], 31_400_000u64);
        assert_eq!(data["installed"], 12_000_000u64);
        assert_eq!(data["root"], "/data/plugins");
        assert_eq!(data["roots"][0]["root"], "/data/plugins");
        assert_eq!(data["roots"][0]["free"], 17_500_000u64);

        // The installed copy is not room: 20 MB free plus the 12 MB an
        // uninstall would return covers the 31.4 MB tree, but nothing is
        // uninstalled before this check runs.
        let error = require_room_at(
            Path::new("/data/plugins"),
            "demo",
            31_400_000,
            12_000_000,
            |_| Some(20_000_000),
        )
        .expect_err("the old copy is not free space");
        assert_eq!(error.problem, Problem::InsufficientSpace);

        // Enough room, or an unmeasurable root, is not a refusal.
        assert!(require_room_at(Path::new("/data"), "demo", 10, 0, |_| Some(11)).is_ok());
        assert!(require_room_at(Path::new("/mnt/sd"), "demo", 10, 0, |_| None).is_ok());
    }

    #[test]
    fn the_refusal_names_every_root_and_what_it_measured() {
        let error = insufficient(
            &[candidate("/tiny", Some(1)), candidate("/gone", None)],
            10,
            "demo",
            4096,
            Some(Path::new("/tiny")),
        );
        let text = error.detail();
        assert!(text.contains("need 10"), "{text}");
        assert!(text.contains("/tiny has 1 bytes"), "{text}");
        assert!(text.contains("/gone is not available"), "{text}");
        assert!(text.contains("/tiny still takes room"), "{text}");
        assert_eq!(error.problem, Problem::InsufficientSpace);

        // One row per candidate, and an unmeasured root stays null rather than
        // being reported as empty.
        let data = error.data().expect("the refusal carries numbers");
        assert_eq!(data["root"], "/tiny");
        assert_eq!(data["roots"][1]["root"], "/gone");
        assert!(data["roots"][1]["free"].is_null(), "{data}");

        // A refusal that never picked a root (fresh install into the smallest
        // volume that fits) reports `root: null`: there is nothing to uninstall.
        let picked = insufficient(&[candidate("/tiny", Some(1))], 10, "demo", 0, None);
        let data = picked.data().expect("the refusal carries numbers");
        assert!(data["root"].is_null(), "{data}");
    }
}
