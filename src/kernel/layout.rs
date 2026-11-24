//! Shared plugin layout and manifest path rules.
//!
//! The plugin directory and the payload-path charset are facts both manage
//! (packaging/installing) and supervise (watching/enabling) need. Keeping them
//! here means neither domain has to reach into the other for a definition.

use std::path::PathBuf;

use crate::kernel::failure::{Problem, ProblemError};
use crate::platform::paths::Paths;

/// The directory that holds one plugin's versions, config and markers.
///
/// A plugin root is a list, so the directory is looked up in the location
/// index first (ADR-0030); a plugin the index does not know falls back to the
/// default root, which keeps the single-root behaviour of ADR-0023.
pub fn plugin_dir(paths: &Paths, id: &str) -> PathBuf {
    crate::platform::locations::dir_of(paths, id)
}

/// A manifest-declared payload path must stay inside the package: relative,
/// forward-slash separated, no empty/`.`/`..` segments.
pub fn check_path(p: &str) -> Result<(), ProblemError> {
    if p.starts_with('/') || p.contains('\\') || p.contains('\0') {
        return Err(Problem::Invalid.error(format!("unsafe path: {}", p)));
    }
    if p.split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(Problem::Invalid.error(format!("unsafe path: {}", p)));
    }
    Ok(())
}
