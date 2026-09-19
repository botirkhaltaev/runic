use std::hint::black_box;

use serde_json::{Value, json};

const ROUNDS: usize = 8;
const USERS: usize = 128;

pub(super) const ELEMENTS: usize = ROUNDS * USERS;

/// JSON API traffic: build a document, serialize, parse, walk.
#[must_use]
pub(super) fn run() -> usize {
    json_api(ROUNDS, USERS)
}

fn json_api(rounds: usize, users: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut items = Vec::with_capacity(users);
        for i in 0..users {
            items.push(json!({
                "id": i ^ round,
                "name": format!("user-{i}"),
                "tags": [round & 3, i % 7, "active"],
                "meta": { "n": i, "round": round },
            }));
        }
        let doc = json!({ "ok": true, "items": items });
        let Ok(text) = serde_json::to_string(&doc) else {
            continue;
        };
        checksum ^= text.len();
        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        checksum ^= parsed["items"].as_array().map_or(0, Vec::len);
        black_box(parsed);
    }
    black_box(checksum)
}
