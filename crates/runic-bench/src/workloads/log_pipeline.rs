use std::{collections::HashMap, hint::black_box, sync::mpsc, thread};

use regex::Regex;

const THREADS: usize = 4;
const ROUNDS: usize = 4;
const LINES: usize = 1_024;

pub(super) const ELEMENTS: usize = ROUNDS * LINES;

/// Log ingest: producer threads format lines, a parser stage extracts fields
/// with `regex`, and an aggregator folds records into a `HashMap` that the main
/// thread drops once the round is done.
#[must_use]
pub(super) fn run() -> usize {
    const LEVELS: [&str; 4] = ["INFO", "WARN", "ERROR", "DEBUG"];
    const SERVICES: usize = 8;

    let Ok(pattern) = Regex::new(r"^(\w+) svc=([a-z]+-\d+) user_(\d+) latency=(\d+)ms$") else {
        return 0;
    };
    let pattern = &pattern;
    let mut checksum = 0_usize;
    for round in 0..ROUNDS {
        let (line_tx, line_rx) = mpsc::channel::<String>();
        let (record_tx, record_rx) = mpsc::channel::<(String, usize)>();
        let totals = thread::scope(|scope| {
            for producer in 0..THREADS {
                let line_tx = line_tx.clone();
                scope.spawn(move || {
                    for i in 0..LINES {
                        let level = LEVELS[(i + producer) % LEVELS.len()];
                        let service = (i + round) % SERVICES;
                        let user = (i * 7 + producer) % 997;
                        let latency = (i % 250) + 1;
                        let line =
                            format!("{level} svc=api-{service} user_{user} latency={latency}ms");
                        if line_tx.send(line).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(line_tx);
            scope.spawn(move || {
                while let Ok(line) = line_rx.recv() {
                    let Some(fields) = pattern.captures(&line) else {
                        continue;
                    };
                    let (Some(level), Some(service), Some(latency)) =
                        (fields.get(1), fields.get(2), fields.get(4))
                    else {
                        continue;
                    };
                    let key = format!("{}/{}", level.as_str(), service.as_str());
                    let millis = latency.as_str().parse::<usize>().unwrap_or(0);
                    if record_tx.send((key, millis)).is_err() {
                        break;
                    }
                }
            });
            let aggregator = scope.spawn(move || {
                let mut totals: HashMap<String, usize> = HashMap::new();
                while let Ok((key, millis)) = record_rx.recv() {
                    *totals.entry(key).or_insert(0) += millis;
                }
                totals
            });
            aggregator.join().ok()
        });
        if let Some(totals) = totals {
            checksum ^= totals.len() ^ totals.values().sum::<usize>();
            black_box(totals);
        }
    }
    black_box(checksum)
}
