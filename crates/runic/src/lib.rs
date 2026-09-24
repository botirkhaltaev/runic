//! Public [`GlobalAlloc`](core::alloc::GlobalAlloc) wrapper for Runic.
//!
//! Published as `runic-alloc`, imported as `runic`. Linux `x86_64`, nightly Rust.
//! C `LD_PRELOAD` is the `runic-cabi` crate, not a feature of this package.

#![deny(unsafe_op_in_unsafe_fn)]

mod global;

pub use global::{RunicAlloc, RunicAllocBuilder};
pub use runic_core::{AllocatorConfig, Budget, ExtentPolicy, RunPolicy};
