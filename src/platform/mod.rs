//! L0 platform: host resources and OS capabilities.
//!
//! The full `platform/` layout in `docs/architecture.md` §5 lands with the
//! directory move. This module currently holds only the pieces that migrated
//! out of the domain layers to break the upward edges `dep-lint` reported
//! (see #137): privilege detection and the target triple.
//!
//! A platform module may hold technical resources, but never a service,
//! capability or call concept (§7 rule 4), and it depends on nothing above it.

pub mod host;

pub use host::privilege::is_privileged;
pub use host::target::target;
