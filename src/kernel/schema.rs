//! Version ceilings the daemon enforces on a `.spk` manifest.
//!
//! Moved out of `package` (#137) because `compat` — a platform module — needs
//! to report them and may not depend on a domain module. The kernel owns the
//! definitions; `package` still decides what a manifest may contain.

/// Highest manifest schema this daemon understands.
pub const MAX_SCHEMA: u32 = 3;
/// Highest plugin protocol version this daemon understands.
pub const MAX_PROTOCOL: u32 = 3;
