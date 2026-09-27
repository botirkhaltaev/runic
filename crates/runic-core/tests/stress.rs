//! Randomised traces with per-block checksums, after mimalloc `test-stress`:
//! every live block carries a pattern, so an overlap, a lost write, a bad
//! realloc copy, or a block handed out twice fails at the next check.

mod common;

use core::alloc::Layout;
use std::sync::mpsc;
use std::thread;

use common::{Block, CLASS_SIZES, EXTENT_SIZES, Rng, layout};
use runic_core::Allocator;

const MAX_SIZE: usize = 96 * 1024;

/// Half the requests sit on or next to a class or extent boundary; the rest
/// are skewed toward small sizes with a tail into extents.
fn trace_size(rng: &mut Rng) -> usize {
    match rng.below(4) {
        0 => {
            let class = CLASS_SIZES[rng.below(CLASS_SIZES.len())];
            (class + rng.below(3)).saturating_sub(1).max(1)
        }
        1 => EXTENT_SIZES[rng.below(2)] + rng.below(3),
        _ => {
            let cap = rng.below(MAX_SIZE).max(16);
            rng.below(cap).max(1)
        }
    }
}

fn is_aligned(ptr: *mut u8, align: usize) -> bool {
    ptr.addr().is_multiple_of(align)
}

fn trace_align(rng: &mut Rng) -> usize {
    const ALIGNS: [usize; 9] = [1, 2, 4, 8, 16, 32, 64, 128, 4096];
    ALIGNS[rng.below(ALIGNS.len())]
}

fn allocate(allocator: &Allocator, rng: &mut Rng, seed: u64) -> Block {
    let layout = layout(trace_size(rng), trace_align(rng));
    let zeroed = rng.below(4) == 0;
    // SAFETY: the layout is valid; the block is freed by whoever holds it.
    let ptr = unsafe {
        if zeroed {
            allocator.alloc_zeroed(layout)
        } else {
            allocator.alloc(layout)
        }
    };
    assert!(!ptr.is_null());
    assert!(is_aligned(ptr, layout.align()));
    if zeroed {
        // SAFETY: live for `layout.size()` bytes.
        let bytes = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
        assert!(
            bytes.iter().all(|&byte| byte == 0),
            "alloc_zeroed left dirt"
        );
    }
    let block = Block { ptr, layout, seed };
    block.fill();
    block
}

fn reallocate(allocator: &Allocator, rng: &mut Rng, block: &mut Block) {
    block.check();
    let old = block.layout;
    let size = trace_size(rng);
    // SAFETY: `block.ptr` is live for `old`; the result replaces it.
    let moved = unsafe { allocator.realloc(block.ptr, old, size) };
    assert!(!moved.is_null());
    assert!(is_aligned(moved, old.align()));
    block.check_prefix(moved, old.size().min(size));
    block.ptr = moved;
    block.layout = Layout::from_size_align(size, old.align()).unwrap();
    block.fill();
}

fn release(allocator: &Allocator, block: &Block) {
    block.check();
    // SAFETY: the block is live and this is its only owner.
    unsafe { allocator.dealloc(block.ptr, block.layout) };
}

/// One thread, one seed: allocation, free, and realloc in random order over a
/// bounded live set, every block checked before it changes hands.
fn single_thread_trace(seed: u64) {
    const OPS: usize = 20_000;
    const MAX_LIVE: usize = 512;
    let allocator = Allocator::new();
    let mut rng = Rng::new(seed);
    let mut live: Vec<Block> = Vec::new();

    for op in 0..OPS {
        let roll = rng.below(100);
        if live.is_empty() || (roll < 60 && live.len() < MAX_LIVE) {
            live.push(allocate(&allocator, &mut rng, seed ^ op as u64));
        } else if roll < 90 {
            let block = live.swap_remove(rng.below(live.len()));
            release(&allocator, &block);
        } else {
            let index = rng.below(live.len());
            reallocate(&allocator, &mut rng, &mut live[index]);
        }
    }

    for block in live {
        release(&allocator, &block);
    }
}

#[test]
fn single_thread_trace_seed_a() {
    single_thread_trace(0xf3ee_a110_c001_cafe);
}

#[test]
fn single_thread_trace_seed_b() {
    single_thread_trace(1);
}

#[test]
fn single_thread_trace_seed_c() {
    single_thread_trace(0xdead_beef);
}

#[test]
fn single_thread_trace_seed_d() {
    single_thread_trace(u64::MAX / 3);
}

/// Several threads run traces at once and pass live blocks around a ring, so
/// free and realloc land on threads that did not allocate the block. Every
/// block is checked when received, when reallocated, and when freed.
#[test]
fn threads_trade_blocks_around_a_ring() {
    const THREADS: usize = 6;
    const OPS: usize = 8_000;
    const MAX_LIVE: usize = 256;
    let allocator = Allocator::new();

    let (senders, receivers): (Vec<_>, Vec<_>) =
        (0..THREADS).map(|_| mpsc::channel::<Block>()).unzip();

    thread::scope(|scope| {
        for (index, receiver) in receivers.into_iter().enumerate() {
            let next = senders[(index + 1) % THREADS].clone();
            let allocator = &allocator;
            scope.spawn(move || {
                let mut rng = Rng::new(0x9000 + index as u64);
                let mut live: Vec<Block> = Vec::new();

                for op in 0..OPS {
                    while let Ok(block) = receiver.try_recv() {
                        block.check();
                        live.push(block);
                    }
                    let roll = rng.below(100);
                    if live.is_empty() || (roll < 50 && live.len() < MAX_LIVE) {
                        live.push(allocate(allocator, &mut rng, (index * OPS + op) as u64));
                    } else if roll < 75 {
                        let block = live.swap_remove(rng.below(live.len()));
                        release(allocator, &block);
                    } else if roll < 90 {
                        let index = rng.below(live.len());
                        reallocate(allocator, &mut rng, &mut live[index]);
                    } else {
                        let block = live.swap_remove(rng.below(live.len()));
                        next.send(block).unwrap();
                    }
                }
                drop(next);

                for block in live {
                    release(allocator, &block);
                }
                // Neighbours may still be sending; take everything until the
                // ring closes, then free it here.
                for block in receiver {
                    release(allocator, &block);
                }
            });
        }
        drop(senders);
    });
}

/// Short-lived owner threads keep handing live blocks to one long-lived
/// thread, which frees them in batches after the owners are gone. Heaps
/// cycle through Active, Draining, and reuse many times.
#[test]
fn short_lived_owners_hand_blocks_to_a_long_lived_freer() {
    const ROUNDS: usize = 40;
    const OWNERS: usize = 4;
    const PER_OWNER: usize = 64;
    let allocator = Allocator::new();
    let mut held: Vec<Block> = Vec::new();
    let mut rng = Rng::new(0x0f0f);

    for round in 0..ROUNDS {
        let mut arrivals = thread::scope(|scope| {
            let owners: Vec<_> = (0..OWNERS)
                .map(|owner| {
                    let allocator = &allocator;
                    scope.spawn(move || {
                        let mut rng = Rng::new((round * OWNERS + owner) as u64);
                        (0..PER_OWNER)
                            .map(|n| allocate(allocator, &mut rng, (round * 1000 + n) as u64))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            owners
                .into_iter()
                .flat_map(|owner| owner.join().unwrap())
                .collect::<Vec<_>>()
        });

        held.append(&mut arrivals);
        // Free about half of what is held, oldest first, and realloc a few.
        let keep = held.len() / 2;
        for block in held.drain(..keep) {
            release(&allocator, &block);
        }
        for _ in 0..8 {
            let index = rng.below(held.len());
            reallocate(&allocator, &mut rng, &mut held[index]);
        }
    }

    for block in held {
        release(&allocator, &block);
    }
}

/// Several gigabytes of checksummed traffic at a bounded resident set.
///
/// Ignored because of the volume, not because it is slow in wall clock: a
/// debug run is on the order of a minute. Hours-long runs are a separate job
/// in mimalloc and do not belong in `cargo test`.
///
/// ```sh
/// cargo test -p runic-core --test stress -- --ignored multi_gigabyte
/// ```
#[test]
#[ignore = "about 4 GiB of checksummed traffic"]
fn multi_gigabyte_trace_keeps_every_block_intact() {
    const BLOCK: usize = 256 * 1024;
    const LIVE: usize = 512;
    const ROUNDS: usize = 32;
    let allocator = Allocator::new();
    let layout = layout(BLOCK, 8);
    let mut held: Vec<Block> = Vec::with_capacity(LIVE);

    for round in 0..ROUNDS {
        for slot in 0..LIVE {
            if held.len() == LIVE {
                release(&allocator, &held.remove(0));
            }
            // SAFETY: the layout is valid; `release` frees the block.
            let ptr = unsafe { allocator.alloc(layout) };
            assert!(!ptr.is_null(), "round {round} slot {slot}");
            let block = Block {
                ptr,
                layout,
                seed: u64::try_from(round * LIVE + slot).unwrap(),
            };
            block.fill();
            held.push(block);
        }
        for block in &held {
            block.check();
        }
    }
    for block in held {
        release(&allocator, &block);
    }
}
