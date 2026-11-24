//! The hook port: manage asks, the supervise runner executes.
//!
//! A package install/remove may declare a signed executable to run. The
//! orchestration (which hook, in what order, what rollback) belongs to manage;
//! spawning, the timeout, the process group and log piping belong to supervise.
//! The port keeps that split without manage importing the runner.

use std::path::Path;

use crate::kernel::failure::ProblemError;

/// Both hook actions share the same 60-second budget.
pub const TIMEOUT_MS: u64 = 60_000;

/// One hook invocation. `action` is `post-install` or `pre-remove`.
#[derive(Debug, Clone, Copy)]
pub struct HookSpec<'a> {
    pub id: &'a str,
    pub version: &'a str,
    pub action: &'a str,
    pub version_dir: &'a Path,
    /// Package-relative path taken from the manifest.
    pub path: &'a str,
    pub timeout_ms: u64,
}

/// The process side of a hook, supplied by the composition root.
pub trait Hooks {
    fn run(&mut self, spec: &HookSpec<'_>) -> Result<(), ProblemError>;
}
