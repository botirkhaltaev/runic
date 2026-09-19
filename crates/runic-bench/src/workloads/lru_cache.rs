use std::{
    collections::{HashMap, VecDeque},
    hint::black_box,
};

use crate::rng::TraceRng;

const ROUNDS: usize = 8;
const ACCESSES: usize = 4_096;

pub(super) const ELEMENTS: usize = ROUNDS * ACCESSES;

pub(super) fn run() -> usize {
    lru_cache(ROUNDS, ACCESSES)
}

/// LRU cache over variable-size strings plus a `VecDeque` recency list.
fn lru_cache(rounds: usize, accesses: usize) -> usize {
    const CAPACITY: usize = 256;
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut rng = TraceRng::new(0x1c0c_ac4e_u64 ^ round as u64);
        let keyspace = (accesses / 2).max(CAPACITY * 4);
        let mut entries: HashMap<usize, String> = HashMap::with_capacity(CAPACITY);
        let mut recency: VecDeque<usize> = VecDeque::with_capacity(CAPACITY);
        let mut hits = 0_usize;
        for i in 0..accesses {
            let key = rng.next_usize(keyspace);
            if let Some(pos) = recency.iter().position(|&k| k == key) {
                recency.remove(pos);
                hits += 1;
            } else if entries.len() >= CAPACITY {
                // `recency` is non-empty whenever the cache is full.
                let victim = recency.pop_front().unwrap_or(key);
                entries.remove(&victim);
            }
            let value = entries
                .entry(key)
                .or_insert_with(|| format!("key={key} {}", "x".repeat(64 + key % 448)));
            value.push(char::from(b'a' + byte(i) % 26));
            value.pop();
            recency.push_back(key);
        }
        checksum ^= hits ^ entries.len() ^ entries.values().map(String::len).sum::<usize>();
        black_box((entries, recency));
    }
    black_box(checksum)
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
