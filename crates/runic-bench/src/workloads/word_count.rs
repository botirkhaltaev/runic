use std::{collections::HashMap, hint::black_box};

use crate::rng::TraceRng;

const ROUNDS: usize = 4;
const WORDS: usize = 4_096;

pub(super) const ELEMENTS: usize = ROUNDS * WORDS;

pub(super) fn run() -> usize {
    word_count(ROUNDS, WORDS)
}

/// Zipf-ish word stream → `HashMap<String, usize>` → top-N.
fn word_count(rounds: usize, words: usize) -> usize {
    const VOCAB: usize = 1_024;
    const TOP: usize = 16;
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut rng = TraceRng::new(0x201d_c001_u64 ^ round as u64);
        let mut map = HashMap::with_capacity(VOCAB);
        for _ in 0..words {
            let span = rng.next_usize(VOCAB).max(1);
            let rank = rng.next_usize(span);
            let word = format!("w{rank}");
            *map.entry(word).or_insert(0) += 1;
        }
        let mut counts: Vec<usize> = map.values().copied().collect();
        counts.sort_unstable();
        checksum ^= counts.iter().rev().take(TOP).sum::<usize>();
        checksum ^= map.len();
        black_box(map);
    }
    black_box(checksum)
}
