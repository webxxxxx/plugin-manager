//! L4 server: protocol sessions and the S4 request-handover seam.

// The HTTP feature supplies the only caller in #113. Keep the S4 contract
// compiled in the minimal build (parent spec #112) without failing dead-code
// analysis for types that are deliberately waiting for #114/#115.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub mod http;
