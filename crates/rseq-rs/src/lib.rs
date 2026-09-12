#![deny(unsafe_op_in_unsafe_fn)]

//! Safe Linux restartable-sequence primitives.
//!
//! Registration only in this slice: [`Rseq`], [`Thread`], [`CpuId`].

mod abi;
mod cpus;
mod membarrier;
mod rseq;
mod thread;

pub use rseq::Rseq;
pub use thread::{CpuId, Thread};
