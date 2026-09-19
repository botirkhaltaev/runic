use std::{cmp::Reverse, collections::BinaryHeap, hint::black_box};

use crate::rng::TraceRng;

const ROUNDS: usize = 4;
const NODES: usize = 1_024;

pub(super) const ELEMENTS: usize = ROUNDS * NODES;

pub(super) fn run() -> usize {
    graph_shortest_path(ROUNDS, NODES)
}

/// Sparse ring-plus-chords graph, then Dijkstra over `BinaryHeap<Reverse<_>>`.
fn graph_shortest_path(rounds: usize, nodes: usize) -> usize {
    const DEGREE: usize = 4;
    let count = nodes.max(2);
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut rng = TraceRng::new(0x9ea0_d151_u64 ^ round as u64);
        let mut adjacency: Vec<Vec<(usize, usize)>> = vec![Vec::new(); count];
        for (from, edges) in adjacency.iter_mut().enumerate() {
            // The ring edge keeps every node reachable; the chords add shortcuts.
            edges.push(((from + 1) % count, 1 + rng.next_usize(16)));
            for step in 1..DEGREE {
                let to = (from * step * 7 + rng.next_usize(count)) % count;
                edges.push((to, 1 + rng.next_usize(64)));
            }
        }

        let start = round % count;
        let mut dist = vec![usize::MAX; count];
        if let Some(slot) = dist.get_mut(start) {
            *slot = 0;
        }
        let mut heap: BinaryHeap<Reverse<(usize, usize)>> = BinaryHeap::new();
        heap.push(Reverse((0, start)));
        let mut settled = 0_usize;
        let mut total = 0_usize;
        while let Some(Reverse((cost, node))) = heap.pop() {
            if dist.get(node).copied().unwrap_or(usize::MAX) < cost {
                continue;
            }
            settled += 1;
            total = total.wrapping_add(cost);
            let Some(edges) = adjacency.get(node) else {
                continue;
            };
            for &(next, weight) in edges {
                let candidate = cost + weight;
                let Some(slot) = dist.get_mut(next) else {
                    continue;
                };
                if candidate < *slot {
                    *slot = candidate;
                    heap.push(Reverse((candidate, next)));
                }
            }
        }
        let reached = dist.iter().filter(|d| **d != usize::MAX).count();
        checksum ^= total ^ settled ^ reached;
        black_box(adjacency);
    }
    black_box(checksum)
}
