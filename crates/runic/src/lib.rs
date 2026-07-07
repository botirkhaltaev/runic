#![deny(unsafe_op_in_unsafe_fn)]

mod global;

pub use global::{ExtentBuilder, RunBuilder, RunicAlloc, RunicAllocBuilder};
pub use runic_core::{AllocatorConfig, Budget, ExtentPolicy, ExtentReuse, RunPolicy};
