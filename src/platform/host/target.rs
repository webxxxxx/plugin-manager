//! The platform id this binary was built for, for example `windows-x86_64`.
//!
//! `build.rs` bakes `SVCD_TARGET` in. Moved out of `package` (#137): a package
//! manifest names a platform, but only the platform layer knows which one it
//! is running on.

/// Exact target triple the binary was built for.
pub fn target() -> &'static str {
    env!("SVCD_TARGET")
}
