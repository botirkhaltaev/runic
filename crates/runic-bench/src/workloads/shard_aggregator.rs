use std::{collections::HashMap, hint::black_box, sync::mpsc, thread};

const THREADS: usize = 4;
const ROUNDS: usize = 4;
const KEYS: usize = 256;
const HITS: usize = 8;

pub(super) const ELEMENTS: usize = ROUNDS * THREADS * KEYS;

/// Workers build `Vec<(String, u64)>` shards and send them to the main thread,
/// which merges counts. Each shard is allocated by a worker and dropped by the
/// aggregator, so frees are remote.
#[must_use]
pub(super) fn run() -> usize {
    let mut checksum = 0_usize;
    for round in 0..ROUNDS {
        let (tx, rx) = mpsc::sync_channel::<Vec<(String, u64)>>(THREADS);
        thread::scope(|scope| {
            for worker in 0..THREADS {
                let posted = tx.clone();
                scope.spawn(move || {
                    let mut shard = Vec::with_capacity(KEYS);
                    for key in 0..KEYS {
                        let name = format!("k{round}-{key}");
                        let count = u64::try_from((key + worker + round) % 17 + 1).unwrap_or(1)
                            * u64::try_from(HITS).unwrap_or(1);
                        shard.push((name, count));
                    }
                    let _ = posted.send(shard);
                });
            }
            drop(tx);
            let mut totals: HashMap<String, u64> = HashMap::with_capacity(KEYS);
            while let Ok(shard) = rx.recv() {
                for (key, count) in shard {
                    *totals.entry(key).or_insert(0) += count;
                }
            }
            checksum ^= totals.len()
                ^ totals
                    .values()
                    .fold(0_usize, |acc, n| acc ^ usize::try_from(*n).unwrap_or(0));
            black_box(totals);
        });
    }
    black_box(checksum)
}
