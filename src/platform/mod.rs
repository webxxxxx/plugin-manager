//! L0 platform: host resources and OS capabilities.
//!
//! The `platform/` layout from `docs/architecture.md` §5 lands with step 6
//! (#170): directories, configuration, host facts and content storage live
//! here. The five-file split of `storage/blob.rs` remains a follow-up ticket.
//!
//! A platform module may hold technical resources, but never a service,
//! capability or call concept (§7 rule 4), and it depends on nothing above it.

pub mod auth;
pub mod clock;
pub mod compat;
pub mod config;
pub mod config_doc;
#[cfg(feature = "http")]
pub mod config_edit;
pub mod config_file;
pub mod host;
pub mod limits;
pub mod locations;
pub mod log;
pub mod paths;
pub mod resources;
pub mod settings;
pub mod storage;
#[cfg(test)]
pub(crate) mod test_support;

pub use host::privilege::is_privileged;
pub use host::target::target;
