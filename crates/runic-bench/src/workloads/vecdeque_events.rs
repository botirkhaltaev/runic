use std::{collections::VecDeque, hint::black_box};

use crate::rng::TraceRng;

const ROUNDS: usize = 8;
const EVENTS: usize = 4_096;

pub(super) const ELEMENTS: usize = ROUNDS * EVENTS;

pub(super) fn run() -> usize {
    vecdeque_events(ROUNDS, EVENTS)
}

/// Bounded event queue: producers push payloads, the consumer drains FIFO in batches.
fn vecdeque_events(rounds: usize, events: usize) -> usize {
    const WINDOW: usize = 512;
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut rng = TraceRng::new(0xe7e0_51d2_u64 ^ round as u64);
        let mut queue: VecDeque<(usize, Vec<u8>)> = VecDeque::new();
        let mut digest = 0_usize;
        for seq in 0..events {
            let payload = vec![byte(seq ^ round); rng.biased_size(256)];
            queue.push_back((seq, payload));
            while queue.len() > WINDOW {
                digest = digest.wrapping_add(deliver(&mut queue));
            }
        }
        while !queue.is_empty() {
            digest = digest.wrapping_add(deliver(&mut queue));
        }
        checksum ^= digest ^ queue.capacity();
        black_box(queue);
    }
    black_box(checksum)
}

fn deliver(queue: &mut VecDeque<(usize, Vec<u8>)>) -> usize {
    let Some((seq, payload)) = queue.pop_front() else {
        return 0;
    };
    seq ^ payload.len() ^ usize::from(payload.first().copied().unwrap_or_default())
}

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
