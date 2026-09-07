mod fan_in;
mod local;
mod owner;
mod remote;
mod reuse;
mod ring;
mod workers;

pub use fan_in::RemoteFanIn;
pub use local::LocalChurn;
pub use owner::OwnerConcurrent;
pub use remote::RemoteFree;
pub use reuse::RemoteReuse;
pub use ring::FreeRing;
pub use workers::{Round, SendPtr, Workers};

use crate::target::AllocatorTarget;

/// Spawn, one local-churn round, join — bind/unbind/drain is the measured path.
///
/// # Panics
///
/// Panics if a worker fails to start or a worker channel is closed.
#[must_use]
pub fn lifecycle(target: AllocatorTarget, threads: usize, ops_per_thread: usize) -> usize {
    let workers = LocalChurn::spawn(target, threads);
    let checksum = workers.run_round(ops_per_thread);
    drop(workers);
    checksum
}
