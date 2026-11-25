//! `GET /api/v1/disk`: the volumes a package touches (#291).
//!
//! One row per plugin root, plus the temp volume as a sibling because a package
//! passes through it. A row's `plugin` field answers "does this root already
//! hold the plugin named by `?id`", which is what lets a caller tell an
//! in-place upgrade from a fresh install that may go anywhere with room. The
//! numbers come from `locations`, the same helper root selection uses, so the
//! pre-check and the install cannot disagree about a volume.

use std::path::{Path, PathBuf};

use super::*;

impl ApiFace {
    /// `GET /api/v1/disk[?id=<plugin id>]`.
    pub(super) fn disk(&self, call: &ApiCall<'_>) -> Response {
        let paths = &self.config.paths;
        let wanted = support::query_value(call.query(), "id").unwrap_or_default();
        if !wanted.is_empty() && !crate::kernel::registry::valid_id(&wanted) {
            return error_response(400, "invalid", "invalid plugin id");
        }
        let roots: Vec<Value> = paths
            .plugin_roots
            .iter()
            .enumerate()
            .map(|(index, root)| {
                json!({
                    "root": root.to_string_lossy(),
                    // `null` when the root is not there: an unmounted volume
                    // must not borrow its parent's free space.
                    "freeBytes": crate::platform::locations::free_bytes(root),
                    "default": index == 0,
                    // What the root already holds: the install picker shows it,
                    // and it is the same count the settings screen locks a root
                    // with (#298/#299).
                    "plugins": crate::platform::locations::ids_in(paths, root).len(),
                    "plugin": (!wanted.is_empty())
                        .then(|| installed_plugin(root, &wanted))
                        .flatten(),
                })
            })
            .collect();
        Response::json(
            200,
            &json!({
                "roots": roots,
                "tempFreeBytes": temp_free_bytes(paths),
                // Whether an install offers the roots or takes what the host
                // picks (#299). Upload and the plugin store read the same flag,
                // so the two surfaces cannot disagree.
                "manualInstallRoot": self.config.settings.manual_install_root.unwrap_or(true),
            }),
        )
    }
}

/// The temp volume is `<data>/temp`; before the first transfer the directory
/// may not exist yet, and then the data root answers for the same volume.
fn temp_free_bytes(paths: &crate::platform::paths::Paths) -> Option<u64> {
    crate::platform::locations::free_bytes(&paths.temp_dir)
        .or_else(|| crate::platform::locations::free_bytes(&paths.data_dir))
}

/// The installed copy of `id` in one root. `current_version_at` is the shared
/// reader, so a row and the scanner agree on what "installed" means.
fn installed_plugin(root: &Path, id: &str) -> Option<Value> {
    let directory: PathBuf = root.join(id);
    let version = crate::manage::model::layout::current_version_at(&directory)?;
    // How much the installed copy occupies: a caller deciding whether an
    // upgrade fits needs this, because the old tree stays until the new one is
    // committed. `null` rather than 0 when it cannot be measured.
    let bytes = crate::manage::installed_bytes(&directory.join(&version));
    Some(json!({
        "id": id,
        "version": version,
        "directory": directory.to_string_lossy(),
        "bytes": bytes,
    }))
}
