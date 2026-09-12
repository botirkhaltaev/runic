#![deny(unsafe_op_in_unsafe_fn)]

//! Safe Linux restartable-sequence primitives.
//!
//! [`Rseq`] registration and x86-64 [`Thread`] word ops.

mod abi;
mod cpus;
mod layout;
mod membarrier;
mod rseq;
mod thread;
mod words;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod x86_64;

pub use rseq::Rseq;
pub use thread::{CpuId, Thread};
pub use words::{Word, Words};
