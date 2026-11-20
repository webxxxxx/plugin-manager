//! Path safety for manifest-declared payloads.

use crate::kernel::failure::{Problem, ProblemError};

pub(crate) fn check_path(p: &str) -> Result<(), ProblemError> {
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
