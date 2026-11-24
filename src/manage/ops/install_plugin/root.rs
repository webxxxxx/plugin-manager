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
        return Ok(recorded);
    }

    if let Some(requested) = requested {
        return Ok(requested);
    }

    let candidates = candidates(paths);
    choose_root(&candidates, needed).ok_or_else(|| insufficient(&candidates, needed))
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
pub(crate) fn candidates(paths: &Paths) -> Vec<Candidate> {
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

fn insufficient(candidates: &[Candidate], needed: u64) -> ProblemError {
    let rows: Vec<String> = candidates
        .iter()
        .map(|(root, free)| match free {
            Some(free) => format!("{} has {} bytes", root.display(), free),
            None => format!("{} is not measurable", root.display()),
        })
        .collect();
    Problem::Internal.error(format!(
        "not enough space to install: need {} bytes and no plugin root can take it ({}); free space on the volume or uninstall an old version first",
        needed,
        rows.join(", ")
    ))
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
    fn paths_at(root: &Path, roots: Vec<PathBuf>) -> Paths {
        let mut paths = Paths::under(root);
        paths.plugin_roots = roots;
        fs::create_dir_all(&paths.data_dir).expect("data dir");
        paths
    }

    #[test]
    fn a_root_outside_the_list_is_a_bad_request() {
        let temp = crate::platform::test_support::TempDir::new();
        let paths = paths_at(&temp.0, vec![temp.0.join("a")]);
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
        let paths = paths_at(&temp.0, vec![first.clone(), second.clone()]);
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

    #[test]
    fn the_refusal_names_every_root_and_what_it_measured() {
        let error = insufficient(&[candidate("/tiny", Some(1)), candidate("/gone", None)], 10);
        let text = error.detail();
        assert!(text.contains("need 10"), "{text}");
        assert!(text.contains("/tiny has 1 bytes"), "{text}");
        assert!(text.contains("/gone is not measurable"), "{text}");
    }
}
