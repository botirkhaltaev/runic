#![deny(unsafe_op_in_unsafe_fn)]

//! Safe Linux restartable-sequence primitives.
//!
//! [`Rseq`] registration and optional [`Words`] region.

mod abi;
mod cpus;
mod layout;
mod membarrier;
mod rseq;
mod thread;
mod words;

pub use rseq::Rseq;
pub use thread::{CpuId, Thread};
pub use words::{Word, Words};
