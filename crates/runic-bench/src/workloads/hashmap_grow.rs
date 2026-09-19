use std::{collections::HashMap, hint::black_box};

use crate::rng::TraceRng;

const ROUNDS: usize = 8;
const KEYS: usize = 4_096;

pub(super) const ELEMENTS: usize = ROUNDS * KEYS;

pub(super) fn run() -> usize {
    hashmap_grow(ROUNDS, KEYS)
}

/// Session store: unreserved `HashMap` growth, lookups, and odd-key removal.
fn hashmap_grow(rounds: usize, keys: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut rng = TraceRng::new(0x5e55_1047_u64 ^ round as u64);
        let mut sessions: HashMap<String, String> = HashMap::new();
        for i in 0..keys {
            sessions.insert(
                format!("sess-{:08x}", i ^ round),
                format!("user={} ttl={}", i % 1_024, (i ^ round) % 8),
            );
        }
        let mut hits = 0_usize;
        for _ in 0..keys {
            let probe = format!("sess-{:08x}", rng.next_usize(keys.max(1) * 2) ^ round);
            hits += usize::from(sessions.contains_key(&probe));
        }
        for i in (1..keys).step_by(2) {
            sessions.remove(&format!("sess-{:08x}", i ^ round));
        }
        checksum = checksum
            .wrapping_add(hits)
            .wrapping_add(sessions.len())
            .wrapping_add(sessions.values().map(String::len).sum::<usize>());
        black_box(sessions);
    }
    black_box(checksum)
}
