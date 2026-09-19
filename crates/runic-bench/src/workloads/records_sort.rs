use std::hint::black_box;

use crate::rng::TraceRng;

const ROUNDS: usize = 8;
const RECORDS: usize = 2_048;

pub(super) const ELEMENTS: usize = ROUNDS * RECORDS;

pub(super) fn run() -> usize {
    records_sort(ROUNDS, RECORDS)
}

struct Record {
    id: usize,
    name: String,
    tags: Vec<String>,
    region: u8,
    score: usize,
}

/// Report pipeline: dedup owned records by id, rank by score, then group by region.
fn records_sort(rounds: usize, records: usize) -> usize {
    const TOP: usize = 16;
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut rng = TraceRng::new(0x5eca_2d51_u64 ^ round as u64);
        let mut rows = Vec::with_capacity(records);
        for i in 0..records {
            let id = rng.next_usize(records.max(1) * 2);
            rows.push(Record {
                id,
                name: format!("acct-{id:06}-{}", i ^ round),
                tags: (0..2 + id % 3)
                    .map(|tag| format!("tag-{}", (tag + i + round) % 31))
                    .collect(),
                region: byte(id) % 8,
                score: rng.next_usize(10_000),
            });
        }
        rows.sort_unstable_by_key(|row| row.id);
        rows.dedup_by_key(|row| row.id);
        rows.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
        let top: usize = rows
            .iter()
            .take(TOP)
            .map(|row| row.score ^ row.name.len() ^ row.tags.iter().map(String::len).sum::<usize>())
            .sum();
        // Stable sort keeps the score ranking inside each region.
        rows.sort_by_key(|row| row.region);
        let regions: usize = rows.iter().map(|row| usize::from(row.region)).sum();
        checksum ^= top ^ rows.len() ^ regions;
        black_box(rows);
    }
    black_box(checksum)
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
