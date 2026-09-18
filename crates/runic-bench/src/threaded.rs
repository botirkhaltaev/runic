use std::{
    collections::HashMap,
    hint::black_box,
    num::NonZero,
    sync::{Arc, mpsc},
    thread,
};

const THREADS: usize = 4;

/// Short-lived threads: each allocates, frees most, and hands a small live set
/// back to the joiner, which drops it after the thread has exited.
///
/// Allocations per thread: `SPAWN_SMALL` 64 B, `SPAWN_LARGE` 1 KiB, `SPAWN_LIVE` kept.
#[must_use]
pub fn spawn_churn(rounds: usize, threads: usize) -> usize {
    const SPAWN_SMALL: usize = 64;
    const SPAWN_LARGE: usize = 8;
    const SPAWN_LIVE: usize = 4;
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let joins: Vec<_> = (0..threads)
            .map(|worker| {
                thread::spawn(move || {
                    let mut small: Vec<Vec<u8>> = (0..SPAWN_SMALL)
                        .map(|i| vec![(i ^ round ^ worker).to_le_bytes()[0]; 64])
                        .collect();
                    let large: Vec<Vec<u8>> = (0..SPAWN_LARGE)
                        .map(|i| vec![(i ^ round).to_le_bytes()[0]; 1024])
                        .collect();
                    let local = small.iter().chain(&large).map(Vec::len).sum::<usize>();
                    small.truncate(SPAWN_LIVE);
                    black_box(large);
                    (local, small)
                })
            })
            .collect();
        for (local, live) in joins.into_iter().filter_map(|join| join.join().ok()) {
            checksum ^= local ^ live.len();
            black_box(live);
        }
    }
    black_box(checksum)
}

/// Allocations per `spawn_churn` thread.
pub const SPAWN_CHURN_ALLOCS: usize = 64 + 8;

/// Threads outnumber cores 4:1. Each thread churns a ring of 64 B blocks so
/// frees hit the freelist in FIFO order, not LIFO.
#[must_use]
pub fn oversubscribed(ops: usize) -> usize {
    const RING: usize = 64;
    let threads = oversubscribed_threads();
    let mut checksum = 0_usize;
    thread::scope(|scope| {
        let joins: Vec<_> = (0..threads)
            .map(|worker| {
                scope.spawn(move || {
                    let mut ring: Vec<Vec<u8>> = (0..RING)
                        .map(|i| vec![(i ^ worker).to_le_bytes()[0]; 64])
                        .collect();
                    let mut local = 0_usize;
                    for i in 0..ops {
                        let slot = i % RING;
                        local ^= usize::from(ring[slot][0]);
                        ring[slot] = vec![(i ^ worker).to_le_bytes()[0]; 64];
                    }
                    black_box(ring);
                    local
                })
            })
            .collect();
        for local in joins.into_iter().filter_map(|join| join.join().ok()) {
            checksum ^= local;
        }
    });
    black_box(checksum)
}

/// `4 * available_parallelism`, at least 8.
#[must_use]
pub fn oversubscribed_threads() -> usize {
    thread::available_parallelism()
        .map_or(2, NonZero::get)
        .saturating_mul(4)
        .max(8)
}

/// Producers allocate `Vec<u8>` / `String` messages; the consumer drops them.
#[must_use]
pub fn channel_pipeline(rounds: usize, messages: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let (tx, rx) = mpsc::channel();
        thread::scope(|scope| {
            for worker in 0..THREADS {
                let tx = tx.clone();
                scope.spawn(move || {
                    for i in 0..messages {
                        let mut payload = Vec::with_capacity(64);
                        payload.extend_from_slice(&(i ^ round ^ worker).to_ne_bytes());
                        payload.extend_from_slice(b"msg");
                        let text = format!("t{worker}-r{round}-{i}");
                        let _ = tx.send((payload, text));
                    }
                });
            }
            drop(tx);
            while let Ok((payload, text)) = rx.recv() {
                checksum ^= payload.len() ^ text.len();
                black_box((payload, text));
            }
        });
    }
    black_box(checksum)
}

/// Shared `Arc<Vec<u8>>` clones; workers drop last so the payload free is remote.
#[must_use]
pub fn arc_share_drop(rounds: usize, clones: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let items: Vec<Arc<Vec<u8>>> = (0..clones)
            .map(|i| Arc::new(vec![(i ^ round).to_le_bytes()[0]; 256]))
            .collect();
        thread::scope(|scope| {
            let stride = clones.div_ceil(THREADS);
            for chunk in items.chunks(stride) {
                let chunk: Vec<Arc<Vec<u8>>> = chunk.to_vec();
                scope.spawn(move || {
                    let mut local = 0_usize;
                    for item in &chunk {
                        local ^= item.len();
                    }
                    black_box((local, chunk));
                });
            }
            drop(items);
        });
        checksum ^= clones;
    }
    black_box(checksum)
}

/// Per-thread maps; main merges and drops worker allocations.
#[must_use]
pub fn scoped_map_reduce(rounds: usize, keys: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let maps = thread::scope(|scope| {
            let mut joins = Vec::with_capacity(THREADS);
            for worker in 0..THREADS {
                joins.push(scope.spawn(move || {
                    let mut map = HashMap::with_capacity(keys);
                    for i in 0..keys {
                        map.insert(format!("t{worker}-k{i}-r{round}"), i ^ round ^ worker);
                    }
                    map
                }));
            }
            joins
                .into_iter()
                .filter_map(|join| join.join().ok())
                .collect::<Vec<_>>()
        });
        for map in maps {
            checksum ^= map.len();
            checksum ^= map.values().sum::<usize>();
            black_box(map);
        }
    }
    black_box(checksum)
}
