use std::{collections::HashMap, hint::black_box, sync::Arc};

use crate::rng::TraceRng;

#[must_use]
pub fn vec_push_clear(rounds: usize, len: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut values = Vec::with_capacity(len);
        for i in 0..len {
            values.push(i ^ round);
        }
        checksum ^= values.iter().copied().sum::<usize>();
        values.clear();
        black_box(values);
    }
    black_box(checksum)
}

#[must_use]
pub fn vec_many_small(rounds: usize, count: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut values = Vec::with_capacity(count);
        for i in 0..count {
            values.push(vec![byte(i ^ round); (i % 128) + 1]);
        }
        checksum ^= values.iter().map(Vec::len).sum::<usize>();
        black_box(values);
    }
    black_box(checksum)
}

#[must_use]
pub fn string_building(rounds: usize, count: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut text = String::new();
        for i in 0..count {
            text.push_str("item");
            text.push_str(&(i ^ round).to_string());
            text.push(';');
        }
        checksum ^= text.len();
        black_box(text);
    }
    black_box(checksum)
}

#[must_use]
pub fn hashmap_insert_remove(rounds: usize, count: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut map = HashMap::with_capacity(count);
        for i in 0..count {
            map.insert(i, i ^ round);
        }
        for i in (0..count).step_by(2) {
            checksum ^= map.remove(&i).unwrap_or_default();
        }
        checksum ^= map.len();
        black_box(map);
    }
    black_box(checksum)
}

#[must_use]
pub fn arc_clone_drop(rounds: usize, count: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let value = Arc::new(vec![byte(round); 1024]);
        let mut clones = Vec::with_capacity(count);
        for _ in 0..count {
            clones.push(Arc::clone(&value));
        }
        checksum ^= Arc::strong_count(&value);
        black_box(clones);
    }
    black_box(checksum)
}

#[must_use]
pub fn mixed_collections(rounds: usize, count: usize) -> usize {
    vec_push_clear(rounds, count)
        ^ vec_many_small(rounds, count / 4)
        ^ string_building(rounds, count / 2)
        ^ hashmap_insert_remove(rounds, count / 2)
        ^ arc_clone_drop(rounds, count / 2)
}

struct Node {
    key: String,
    value: usize,
    children: Vec<Node>,
}

/// JSON-ish tree: `Node` with `String` keys and `Vec` children, build/mutate/drop.
#[must_use]
pub fn tree(rounds: usize, nodes: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut remaining = nodes;
        let mut id = round;
        let mut root = build_node(&mut remaining, &mut id, 4);
        mutate(&mut root, round);
        checksum ^= walk(&root);
        black_box(root);
    }
    black_box(checksum)
}

fn build_node(remaining: &mut usize, id: &mut usize, width: usize) -> Node {
    *id += 1;
    let key = format!("n{id}");
    let mut children = Vec::new();
    let n = width.min(*remaining);
    for _ in 0..n {
        if *remaining == 0 {
            break;
        }
        *remaining -= 1;
        children.push(build_node(remaining, id, width));
    }
    Node {
        key,
        value: *id,
        children,
    }
}

fn mutate(node: &mut Node, tag: usize) {
    node.value ^= tag;
    node.key.push_str("-m");
    for child in &mut node.children {
        mutate(child, tag);
    }
}

fn walk(node: &Node) -> usize {
    node.value ^ node.key.len() ^ node.children.iter().map(walk).sum::<usize>()
}

/// Zipf-ish word stream → `HashMap<String, usize>` → top-N.
#[must_use]
pub fn word_count(rounds: usize, words: usize) -> usize {
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

fn byte(value: usize) -> u8 {
    value.to_le_bytes()[0]
}
