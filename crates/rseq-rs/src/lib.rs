#![deny(unsafe_op_in_unsafe_fn)]

//! Safe Linux restartable-sequence primitives.
//!
//! [`Rseq`] registration and portable [`LockedStacks`].

mod abi;
mod cpus;
mod layout;
mod locked;
mod membarrier;
mod rseq;
mod thread;

pub use locked::{CpuStacks, Full, LockedStacks};
pub use rseq::Rseq;
pub use thread::{CpuId, Thread};
