use std::hint::black_box;

use bytes::{BufMut, BytesMut};
use regex::Regex;
use serde_json::{Value, json};

/// JSON API traffic: build a document, serialize, parse, walk.
#[must_use]
pub fn json_api(rounds: usize, users: usize) -> usize {
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

/// Log-scan traffic: compile a pattern and search generated lines.
#[must_use]
pub fn regex_search(rounds: usize, lines: usize) -> usize {
    let Ok(re) = Regex::new(r"(?i)\b(error|warn|user_\d+)\b") else {
        return 0;
    };
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut haystack = String::with_capacity(lines * 40);
        for i in 0..lines {
            haystack.push_str(match (i ^ round) % 5 {
                0 => "ERROR user_",
                1 => "WARN cache miss user_",
                2 => "INFO accepted user_",
                _ => "DEBUG skip user_",
            });
            haystack.push_str(&(i % 97).to_string());
            haystack.push('\n');
        }
        checksum ^= re.find_iter(&haystack).count();
        black_box(haystack);
    }
    black_box(checksum)
}

/// Network buffer traffic: append chunks, split, freeze (`bytes`).
#[must_use]
pub fn http_buffers(rounds: usize, chunks: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut buf = BytesMut::with_capacity(chunks * 64);
        for i in 0..chunks {
            buf.put_slice(&(i ^ round).to_ne_bytes());
            buf.put_slice(b"GET /item/");
            buf.put_slice((i % 1000).to_string().as_bytes());
            buf.put_slice(b"\r\n");
        }
        checksum ^= buf.len();
        while buf.len() > 16 {
            let part = buf.split_to(16);
            checksum ^= part.len();
            black_box(part.freeze());
        }
        if !buf.is_empty() {
            checksum ^= buf.len();
            black_box(buf.freeze());
        }
    }
    black_box(checksum)
}

/// Request-sized read/decode buffers, 64 KiB through 1 MiB.
#[must_use]
pub fn large_buffers(rounds: usize, requests: usize) -> usize {
    const PAGE: usize = 64 * 1024;
    let mut checksum = 0_usize;
    for round in 0..rounds {
        for i in 0..requests {
            let pages = (i % 16) + 1;
            let len = pages * PAGE;
            let mut buf = vec![0_u8; len];
            buf[0] = (i ^ round).to_le_bytes()[0];
            buf[len - 1] = round.to_le_bytes()[0];
            checksum ^= buf.len() ^ usize::from(buf[0]);
            black_box(buf);
        }
    }
    black_box(checksum)
}

/// Same sizes as [`large_buffers`], but `with_capacity` + fill (no `alloc_zeroed`).
#[must_use]
pub fn large_buffers_dirty(rounds: usize, requests: usize) -> usize {
    const PAGE: usize = 64 * 1024;
    let mut checksum = 0_usize;
    for round in 0..rounds {
        for i in 0..requests {
            let pages = (i % 16) + 1;
            let len = pages * PAGE;
            let mut buf = Vec::with_capacity(len);
            buf.resize(len, 0x5a);
            buf[0] = (i ^ round).to_le_bytes()[0];
            buf[len - 1] = round.to_le_bytes()[0];
            checksum ^= buf.len() ^ usize::from(buf[0]);
            black_box(buf);
        }
    }
    black_box(checksum)
}
