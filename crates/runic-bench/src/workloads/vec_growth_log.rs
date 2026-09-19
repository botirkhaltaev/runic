use std::hint::black_box;

use crate::rng::TraceRng;

const ROUNDS: usize = 16;
const LINES: usize = 1_024;

pub(super) const ELEMENTS: usize = ROUNDS * LINES;

pub(super) fn run() -> usize {
    vec_growth_log(ROUNDS, LINES)
}

/// Append-only log: grow a `Vec<String>` with no reservation, then compact to errors.
fn vec_growth_log(rounds: usize, lines: usize) -> usize {
    const LEVELS: [&str; 4] = ["DEBUG", "INFO", "WARN", "ERROR"];
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut rng = TraceRng::new(0x10c1_9a11_u64 ^ round as u64);
        let mut log: Vec<String> = Vec::new();
        for i in 0..lines {
            let level = LEVELS[rng.next_usize(LEVELS.len())];
            let ts = round * lines + i;
            log.push(format!(
                "t={ts} lvl={level} req={} shard={}",
                i ^ round,
                i % 37
            ));
            if i != 0 && i % 256 == 0 {
                log.shrink_to_fit();
            }
        }
        let bytes: usize = log.iter().map(String::len).sum();
        log.retain(|line| line.contains("ERROR"));
        checksum ^= bytes ^ log.len() ^ log.capacity();
        black_box(log);
    }
    black_box(checksum)
}
