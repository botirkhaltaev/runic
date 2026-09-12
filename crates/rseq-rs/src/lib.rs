#![deny(unsafe_op_in_unsafe_fn)]

//! Safe Linux restartable-sequence primitives.
//!
//! [`Rseq`] registration, [`LockedStacks`], and x86-64 [`Stacks`].

mod abi;
mod cpus;
mod layout;
mod locked;
mod membarrier;
mod rseq;
mod stacks;
mod thread;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod x86_64;

pub use locked::{CpuStacks, Full, LockedStacks};
pub use rseq::Rseq;
pub use stacks::Stacks;
pub use thread::{CpuId, Thread};
