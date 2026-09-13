#![deny(unsafe_op_in_unsafe_fn)]

//! Safe Linux restartable-sequence primitives.
//!
//! [`Rseq`] registration and [`Thread`] word ops. Keep [`Words`] alive across
//! [`Words::get`]; on [`Error::Abort`] re-read [`Thread::cpu_id`] and pick a
//! new word — do not retry the same [`Word`].
//!
//! ```
//! # fn try_it() -> Option<()> {
//! use rseq_rs::{Error, Rseq};
//! let rseq = Rseq::try_new()?;
//! let t = rseq.bind()?;
//! let words = rseq.words()?;
//! loop {
//!     let cpu = t.cpu_id()?;
//!     let w = words.get(cpu)?;
//!     match t.compare_exchange(w, 0, 7) {
//!         Ok(_) | Err(Error::Miss(_)) => break,
//!         Err(Error::Abort) => {}
//!     }
//! }
//! # Some(())
//! # }
//! # let _ = try_it();
//! ```

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
pub use thread::{CpuId, Error, Thread};
pub use words::{Word, Words};
