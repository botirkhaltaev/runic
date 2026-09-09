use std::{
    collections::HashMap,
    hint::black_box,
    sync::{Arc, mpsc},
    thread,
};

const THREADS: usize = 4;

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
