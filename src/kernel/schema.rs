//! Version ceilings the daemon enforces on a `.spk` manifest.
//!
//! Moved out of `package` (#137) because `compat` — a platform module — needs
//! to report them and may not depend on a domain module. The kernel owns the
//! definitions; `package` still decides what a manifest may contain.

/// Highest manifest schema this daemon understands.
///
/// The format was renumbered when `id`, `outputSchema` and `icons` landed
/// (#260): there is exactly one version, `1`, and everything an older line put
/// behind schema 2/3 is part of it. A package whose `schema` is anything else
/// is rejected at parse time.
pub const MAX_SCHEMA: u32 = 1;
/// Highest plugin protocol version this daemon understands.
pub const MAX_PROTOCOL: u32 = 3;
