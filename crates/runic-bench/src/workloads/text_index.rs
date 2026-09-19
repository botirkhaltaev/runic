use std::{collections::BTreeMap, hint::black_box};

use crate::rng::TraceRng;

const ROUNDS: usize = 4;
const DOCS: usize = 4_096;

pub(super) const ELEMENTS: usize = ROUNDS * DOCS;

pub(super) fn run() -> usize {
    text_index(ROUNDS, DOCS)
}

/// Inverted index: tokenize synthetic documents into postings, then answer AND queries.
fn text_index(rounds: usize, docs: usize) -> usize {
    const VOCAB: usize = 512;
    const TERMS_PER_DOC: usize = 24;
    const QUERIES: usize = 16;
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut rng = TraceRng::new(0x7ea7_1de0_u64 ^ round as u64);
        let mut index: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for doc in 0..docs {
            let doc_id = u32::try_from(doc).unwrap_or(u32::MAX);
            let mut text = String::new();
            for _ in 0..TERMS_PER_DOC {
                text.push('t');
                text.push_str(&rng.next_usize(VOCAB).to_string());
                text.push(' ');
            }
            for term in text.split_whitespace() {
                let postings = index.entry(term.to_owned()).or_default();
                if postings.last() != Some(&doc_id) {
                    postings.push(doc_id);
                }
            }
            black_box(text);
        }
        let mut matched = 0_usize;
        for q in 0..QUERIES {
            let Some(left) = index.get(&format!("t{q}")) else {
                continue;
            };
            let Some(right) = index.get(&format!("t{}", q + 1)) else {
                continue;
            };
            matched += left
                .iter()
                .filter(|&&doc| right.binary_search(&doc).is_ok())
                .count();
        }
        checksum ^= matched ^ index.len() ^ index.values().map(Vec::len).sum::<usize>();
        black_box(index);
    }
    black_box(checksum)
}
