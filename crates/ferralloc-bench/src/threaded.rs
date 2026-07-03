use std::{
    alloc::Layout,
    hint::black_box,
    sync::{Arc, Barrier, mpsc},
    thread,
};

use crate::{allocator_target::AllocatorTarget, rng::TraceRng, workload};

struct SendPtr(*mut u8);

unsafe impl Send for SendPtr {}

/// Runs per-thread local allocation churn.
///
/// # Panics
///
/// Panics if a worker thread panics.
#[must_use]
pub fn thread_local_churn(target: AllocatorTarget, threads: usize, ops_per_thread: usize) -> usize {
    thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::with_capacity(threads);

        for index in 0..threads {
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                barrier.wait();
                workload::single_size_churn(target, 64 + index * 8, ops_per_thread)
            }));
        }

        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum()
    })
}

/// Sends allocations around a thread ring and frees them on another thread.
///
/// # Panics
///
/// Panics if layout construction, allocation, channel operations, or thread joins fail.
#[must_use]
pub fn cross_thread_free_ring(
    target: AllocatorTarget,
    threads: usize,
    ops_per_thread: usize,
) -> usize {
    let layout = Layout::from_size_align(64, 8).unwrap();

    thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(threads));
        let mut senders = Vec::with_capacity(threads);
        let mut receivers = Vec::with_capacity(threads);

        for _ in 0..threads {
            let (tx, rx) = mpsc::channel::<SendPtr>();
            senders.push(tx);
            receivers.push(Some(rx));
        }

        let mut handles = Vec::with_capacity(threads);
        for index in 0..threads {
            let tx = senders[(index + 1) % threads].clone();
            let rx = receivers[index].take().unwrap();
            let barrier = Arc::clone(&barrier);

            handles.push(scope.spawn(move || {
                barrier.wait();
                let mut checksum = 0_usize;
                for i in 0..ops_per_thread {
                    let ptr = target.alloc(black_box(layout));
                    unsafe { ptr.as_ptr().write(byte(i)) };
                    tx.send(SendPtr(ptr.as_ptr())).unwrap();
                    let received = rx.recv().unwrap();
                    checksum ^= received.0 as usize;
                    let ptr = std::ptr::NonNull::new(received.0).unwrap();
                    target.dealloc(ptr, layout);
                }
                checksum
            }));
        }

        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum()
    })
}

/// Runs randomized small-allocation traces on multiple threads.
///
/// # Panics
///
/// Panics if a worker thread panics.
#[must_use]
pub fn mixed_thread_random(
    target: AllocatorTarget,
    threads: usize,
    ops_per_thread: usize,
) -> usize {
    thread::scope(|scope| {
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::with_capacity(threads);

        for index in 0..threads {
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                barrier.wait();
                let mut rng = TraceRng::new(0x9e37_79b9_7f4a_7c15 ^ index as u64);
                let ops = ops_per_thread + rng.next_usize(8);
                workload::small_biased_random(target, rng.next_u64(), ops, 128)
            }));
        }

        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum()
    })
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
