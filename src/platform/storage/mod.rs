//! Content-addressed storage (formerly `blob.rs`): upload sessions, parts and
//! expiry. A technical resource, so it lives under `platform` (§5).
//!
//! The `meta / path / upload / part / gc` split is a follow-up ticket; step 6
//! moved the file without cutting it.

pub mod blob;
