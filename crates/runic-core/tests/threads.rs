//! Cross-thread contract: remote free, owner exit, and Draining heaps. Each
//! test ends by allocating again so a block stranded anywhere shows up as
//! lost reuse or a fault, never as silent success.

mod common;

use std::collections::HashSet;
use std::sync::{Barrier, mpsc};
use std::thread;
use std::time::Duration;

use common::{Block, CLASS_SIZES, EXTENT_SIZES, Rng, layout};
use runic_core::Allocator;

/// Blocks of every size stay valid after their owner thread exits, and any
/// thread may free them.
#[test]
fn blocks_of_every_size_outlive_their_owner_thread() {
    let allocator = Allocator::new();

    let blocks = thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut blocks = Vec::new();
                for (seed, size) in CLASS_SIZES.into_iter().chain(EXTENT_SIZES).enumerate() {
                    let layout = layout(size, 8);
                    // SAFETY: the layout is valid; the main thread frees the block.
                    let ptr = unsafe { allocator.alloc(layout) };
                    assert!(!ptr.is_null(), "size {size}");
                    let block = Block {
                        ptr,
                        layout,
                        seed: seed as u64,
                    };
                    block.fill();
                    blocks.push(block);
                }
                blocks
            })
            .join()
            .unwrap()
    });

    for block in blocks {
        block.check();
        // SAFETY: allocated on the exited thread with the same layout.
        unsafe { allocator.dealloc(block.ptr, block.layout) };
    }
}

/// An owner that exits with thousands of live blocks across classes and
/// extents leaves every one of them usable and freeable in any order.
#[test]
fn owner_exit_with_many_live_blocks_frees_cleanly_later() {
    const PER_SIZE: usize = 200;
    let allocator = Allocator::new();
    let mut rng = Rng::new(0x0e_1f);

    let blocks = thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut blocks = Vec::new();
                for (seed, size) in CLASS_SIZES.into_iter().chain([EXTENT_SIZES[0]]).enumerate() {
                    for _ in 0..PER_SIZE {
                        let layout = layout(size, 8);
                        // SAFETY: the layout is valid; the main thread frees the block.
                        let ptr = unsafe { allocator.alloc(layout) };
                        assert!(!ptr.is_null(), "size {size}");
                        let block = Block {
                            ptr,
                            layout,
                            seed: seed as u64,
                        };
                        block.fill();
                        blocks.push(block);
                    }
                }
                blocks
            })
            .join()
            .unwrap()
    });

    let mut blocks = blocks;
    while !blocks.is_empty() {
        let block = blocks.swap_remove(rng.below(blocks.len()));
        block.check();
        // SAFETY: allocated on the exited thread with the same layout.
        unsafe { allocator.dealloc(block.ptr, block.layout) };
    }

    // The draining heaps are gone; fresh allocation still works.
    let layout = layout(64, 8);
    // SAFETY: valid layout, freed immediately.
    unsafe {
        let ptr = allocator.alloc(layout);
        assert!(!ptr.is_null());
        allocator.dealloc(ptr, layout);
    }
}

/// Remote frees spanning more runs than a freer holds open slots for all
/// reach the owner no later than the freer exiting: the owner then refills
/// every run with exactly the blocks that were freed.
#[test]
fn remote_frees_across_many_runs_all_return_to_the_owner() {
    const RUN: usize = 64 * 1024;
    let allocator = Allocator::new();
    let classes = &CLASS_SIZES[..12];
    let (blocks_tx, blocks_rx) = mpsc::channel::<Vec<Vec<Block>>>();
    let allocator = &allocator;

    thread::scope(|scope| {
        let freer = scope.spawn(move || {
            // Round-robin over classes so the freer rotates through more
            // runs than it has slots, evicting partial chains as it goes.
            let mut per_class = blocks_rx.recv().unwrap();
            while per_class.iter().any(|blocks| !blocks.is_empty()) {
                for blocks in &mut per_class {
                    if let Some(block) = blocks.pop() {
                        block.check();
                        // SAFETY: allocated by the owner with the same layout.
                        unsafe { allocator.dealloc(block.ptr, block.layout) };
                    }
                }
            }
        });

        scope.spawn(move || {
            // Fill one whole run per class so the owner must miss, and
            // therefore flush its inbox, before it can allocate again.
            let mut per_class = Vec::new();
            for (seed, &size) in classes.iter().enumerate() {
                let layout = layout(size, 8);
                let blocks: Vec<Block> = (0..RUN / size)
                    .map(|_| {
                        // SAFETY: the layout is valid; the freer frees the block.
                        let ptr = unsafe { allocator.alloc(layout) };
                        assert!(!ptr.is_null(), "size {size}");
                        let block = Block {
                            ptr,
                            layout,
                            seed: seed as u64,
                        };
                        block.fill();
                        block
                    })
                    .collect();
                per_class.push(blocks);
            }
            let handed_over: HashSet<usize> = per_class
                .iter()
                .flatten()
                .map(|block| block.ptr.addr())
                .collect();
            blocks_tx.send(per_class).unwrap();
            freer.join().unwrap();

            let mut reused = HashSet::new();
            for &size in classes {
                let layout = layout(size, 8);
                let again: Vec<*mut u8> = (0..RUN / size)
                    .map(|_| {
                        // SAFETY: the layout is valid and the block is freed below.
                        let ptr = unsafe { allocator.alloc(layout) };
                        assert!(!ptr.is_null(), "size {size}");
                        reused.insert(ptr.addr());
                        ptr
                    })
                    .collect();
                for ptr in again {
                    // SAFETY: allocated just above with the same layout.
                    unsafe { allocator.dealloc(ptr, layout) };
                }
            }
            assert_eq!(
                reused, handed_over,
                "the owner did not get every remote free back"
            );
        });
    });
}

/// A block allocated on one thread can be reallocated on another through
/// every size step, keeping its prefix, and freed there.
#[test]
fn realloc_from_another_thread_keeps_the_prefix() {
    let allocator = Allocator::new();

    let mut block = thread::scope(|scope| {
        scope
            .spawn(|| {
                let layout = layout(24, 8);
                let block = Block {
                    // SAFETY: the layout is valid; the main thread takes over.
                    ptr: unsafe { allocator.alloc(layout) },
                    layout,
                    seed: 3,
                };
                assert!(!block.ptr.is_null());
                block.fill();
                block
            })
            .join()
            .unwrap()
    });

    for size in [24, 200, 4096, 100 * 1024, 512, 8] {
        let old = block.layout;
        // SAFETY: `block.ptr` is live for `old`; the result replaces it.
        let moved = unsafe { allocator.realloc(block.ptr, old, size) };
        assert!(!moved.is_null(), "size {size}");
        block.check_prefix(moved, old.size().min(size));
        block.ptr = moved;
        block.layout = layout(size, 8);
        block.fill();
    }
    // SAFETY: the final pointer is live for the final layout.
    unsafe { allocator.dealloc(block.ptr, block.layout) };
}

/// A freer claims while the owner is Active, then the owner exits. The freer
/// exiting later must not lose the block: the owner heap drains it.
#[test]
fn claim_made_while_active_survives_the_owner_exiting() {
    let allocator = Allocator::new();
    let layout = layout(64, 8);
    let (ptr_tx, ptr_rx) = mpsc::channel();
    let (freed_tx, freed_rx) = mpsc::channel();
    let (owner_done_tx, owner_done_rx) = mpsc::channel();
    let allocator = &allocator;

    thread::scope(|scope| {
        scope.spawn(move || {
            // SAFETY: the layout is valid; the freer frees the block.
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null());
            // SAFETY: live for 64 bytes.
            unsafe { ptr.write(0x5a) };
            ptr_tx.send(ptr.addr()).unwrap();
            freed_rx.recv().unwrap();
            owner_done_tx.send(()).unwrap();
        });

        scope.spawn(move || {
            let ptr = core::ptr::with_exposed_provenance_mut::<u8>(ptr_rx.recv().unwrap());
            // SAFETY: the owner wrote this byte and handed the block over.
            assert_eq!(unsafe { ptr.read() }, 0x5a);
            // SAFETY: allocated by the owner with the same layout.
            unsafe { allocator.dealloc(ptr, layout) };
            freed_tx.send(()).unwrap();
            owner_done_rx.recv().unwrap();
        });
    });

    // SAFETY: valid layout, freed immediately.
    unsafe {
        let ptr = allocator.alloc(layout);
        assert!(!ptr.is_null());
        ptr.write(0xa5);
        assert_eq!(ptr.read(), 0xa5);
        allocator.dealloc(ptr, layout);
    }
}

/// A thread that never allocated (never bound a heap) frees blocks of an
/// exited owner: each free reaches the Draining heap directly.
#[test]
fn unbound_freer_completes_remote_frees_to_a_draining_owner() {
    let allocator = Allocator::new();
    let layout = layout(64, 8);

    let blocks = thread::scope(|scope| {
        scope
            .spawn(|| {
                (0..8_u64)
                    .map(|seed| {
                        let block = Block {
                            // SAFETY: the layout is valid; the freer frees it.
                            ptr: unsafe { allocator.alloc(layout) },
                            layout,
                            seed,
                        };
                        assert!(!block.ptr.is_null());
                        block.fill();
                        block
                    })
                    .collect::<Vec<_>>()
            })
            .join()
            .unwrap()
    });

    thread::scope(|scope| {
        scope.spawn(|| {
            for block in blocks {
                block.check();
                // SAFETY: allocated by the exited owner with the same layout.
                unsafe { allocator.dealloc(block.ptr, block.layout) };
            }
        });
    });

    // SAFETY: valid layout, freed immediately.
    unsafe {
        let ptr = allocator.alloc(layout);
        assert!(!ptr.is_null());
        allocator.dealloc(ptr, layout);
    }
}

/// A bound freer enqueues one block while the owner is Active, the owner
/// exits, then the freer frees a second block of the same run into the
/// Draining heap. Neither block is stranded.
#[test]
fn bound_freer_spans_the_owner_going_from_active_to_draining() {
    let allocator = Allocator::new();
    let layout = layout(64, 8);
    let (ptrs_tx, ptrs_rx) = mpsc::channel();
    let (hold_tx, hold_rx) = mpsc::channel();
    let allocator = &allocator;

    thread::scope(|scope| {
        let owner = scope.spawn(move || {
            // SAFETY: the layouts are valid; the freer frees both blocks.
            let a = unsafe { allocator.alloc(layout) };
            let b = unsafe { allocator.alloc(layout) };
            assert!(!a.is_null() && !b.is_null());
            ptrs_tx.send((a.addr(), b.addr())).unwrap();
            hold_rx.recv().unwrap();
        });

        scope.spawn(move || {
            // Bind this thread's heap so the first free goes through a slot.
            // SAFETY: valid layout, freed immediately.
            unsafe {
                let binder = allocator.alloc(layout);
                assert!(!binder.is_null());
                allocator.dealloc(binder, layout);
            }

            let (a, b) = ptrs_rx.recv().unwrap();
            // SAFETY: allocated by the owner with the same layout.
            unsafe { allocator.dealloc(core::ptr::with_exposed_provenance_mut(a), layout) };
            hold_tx.send(()).unwrap();
            owner.join().unwrap();
            // SAFETY: allocated by the owner with the same layout.
            unsafe { allocator.dealloc(core::ptr::with_exposed_provenance_mut(b), layout) };
        });
    });

    // SAFETY: valid layout, freed immediately.
    unsafe {
        let ptr = allocator.alloc(layout);
        assert!(!ptr.is_null());
        allocator.dealloc(ptr, layout);
    }
}

/// Several freers hammer one Active owner at once; afterwards the owner can
/// allocate and free the same count again.
#[test]
fn concurrent_freers_against_one_active_owner() {
    const FREERS: usize = 4;
    const PER_FREER: usize = 256;
    let allocator = Allocator::new();
    let layout = layout(64, 8);
    let (ready_tx, ready_rx) = mpsc::channel();
    let start = Barrier::new(FREERS + 1);
    let done = Barrier::new(FREERS + 1);
    let allocator = &allocator;

    thread::scope(|scope| {
        scope.spawn(|| {
            let blocks: Vec<Block> = (0..(FREERS * PER_FREER) as u64)
                .map(|seed| {
                    let block = Block {
                        // SAFETY: the layout is valid; a freer frees it.
                        ptr: unsafe { allocator.alloc(layout) },
                        layout,
                        seed,
                    };
                    assert!(!block.ptr.is_null());
                    block.fill();
                    block
                })
                .collect();
            ready_tx.send(blocks).unwrap();
            start.wait();
            // Stay Active, and idle, until every freer has finished.
            done.wait();

            for _ in 0..FREERS * PER_FREER {
                // SAFETY: valid layout, freed immediately.
                unsafe {
                    let ptr = allocator.alloc(layout);
                    assert!(!ptr.is_null());
                    allocator.dealloc(ptr, layout);
                }
            }
        });

        let mut blocks = ready_rx.recv().unwrap();
        for _ in 0..FREERS {
            let chunk: Vec<Block> = blocks.drain(..PER_FREER).collect();
            let (start, done) = (&start, &done);
            scope.spawn(move || {
                start.wait();
                for block in chunk {
                    block.check();
                    // SAFETY: allocated by the owner with the same layout.
                    unsafe { allocator.dealloc(block.ptr, block.layout) };
                }
                done.wait();
            });
        }
    });
}

/// Regression for the remote-free livelock (#61): a burst far larger than
/// any internal queue returns while the owner makes no allocator progress.
#[test]
fn remote_free_burst_completes_without_owner_progress() {
    const BURST: usize = 4 * 1024;
    const TIMEOUT: Duration = Duration::from_secs(10);
    let allocator = Allocator::new();
    let layout = layout(64, 8);
    let (blocks_tx, blocks_rx) = mpsc::channel();
    let (park_tx, park_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let allocator = &allocator;

    thread::scope(|scope| {
        scope.spawn(move || {
            let blocks: Vec<Block> = (0..BURST as u64)
                .map(|seed| {
                    let block = Block {
                        // SAFETY: the layout is valid; the freer frees it.
                        ptr: unsafe { allocator.alloc(layout) },
                        layout,
                        seed,
                    };
                    assert!(!block.ptr.is_null());
                    block.fill();
                    block
                })
                .collect();
            blocks_tx.send(blocks).unwrap();
            park_rx.recv().unwrap();
        });

        scope.spawn(move || {
            for block in blocks_rx.recv().unwrap() {
                block.check();
                // SAFETY: allocated by the owner with the same layout.
                unsafe { allocator.dealloc(block.ptr, block.layout) };
            }
            // SAFETY: valid layout, freed immediately.
            unsafe {
                let ptr = allocator.alloc(layout);
                assert!(!ptr.is_null());
                allocator.dealloc(ptr, layout);
            }
            park_tx.send(()).unwrap();
            done_tx.send(()).unwrap();
        });

        done_rx
            .recv_timeout(TIMEOUT)
            .expect("remote free burst did not complete");
    });
}
