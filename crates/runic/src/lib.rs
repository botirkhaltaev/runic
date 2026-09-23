#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(any(test, feature = "c-abi"))]
mod cabi;
mod global;

pub use global::{RunicAlloc, RunicAllocBuilder};
pub use runic_core::{AllocatorConfig, Budget, ExtentPolicy, RunPolicy};
