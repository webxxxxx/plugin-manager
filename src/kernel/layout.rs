//! Shared plugin layout and manifest path rules.
//!
//! The plugin directory and the payload-path charset are facts both manage
//! (packaging/installing) and supervise (watching/enabling) need. Keeping them
//! here means neither domain has to reach into the other for a definition.

use std::path::PathBuf;

use crate::kernel::failure::{Problem, ProblemError};
use crate::platform::paths::Paths;

/// The directory that holds one plugin's versions, config and markers.
pub fn plugin_dir(paths: &Paths, id: &str) -> PathBuf {
    paths.plugins_dir.join(id)
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
