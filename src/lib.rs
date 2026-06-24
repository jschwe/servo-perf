//! Library re-exports for the small auxiliary binaries (e.g. `pftrace_stats`)
//! that want to reuse the `trace` module's `parse` function without duplicating it.
//!
//! Keep this surface area minimal — anything else should stay private to the
//! `servoperf` binary in `main.rs`.

#[cfg(feature = "pftrace")]
pub mod proto;
pub mod trace;
