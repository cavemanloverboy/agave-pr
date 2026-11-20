//! Re-exports the crates.io solana-sbpf 0.28.0 release, or the J6 JIT build when the `j6` feature
//! is enabled. Cargo unifies features, so enabling `j6` anywhere in the build switches every crate.

#[cfg(not(feature = "j6"))]
pub use solana_sbpf_prod::*;

#[cfg(feature = "j6")]
pub use solana_sbpf_j6::*;
