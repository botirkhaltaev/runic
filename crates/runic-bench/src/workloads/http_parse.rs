use std::collections::HashMap;
use std::hint::black_box;

use bytes::{BufMut, BytesMut};

const ROUNDS: usize = 16;
const REQUESTS: usize = 256;

pub(super) const ELEMENTS: usize = ROUNDS * REQUESTS;

/// HTTP request parse + header map + `bytes` response body.
#[must_use]
pub(super) fn run() -> usize {
    http_parse(ROUNDS, REQUESTS)
}

fn http_parse(rounds: usize, requests: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        for i in 0..requests {
            let mut raw = BytesMut::with_capacity(192);
            raw.put_slice(b"GET /api/items/");
            raw.put_slice((i % 10_000).to_string().as_bytes());
            raw.put_slice(b"?round=");
            raw.put_slice(round.to_string().as_bytes());
            raw.put_slice(b" HTTP/1.1\r\nHost: bench.local\r\nAccept: application/json\r\n");
            raw.put_slice(b"X-Request-Id: ");
            raw.put_slice((i ^ round).to_string().as_bytes());
            raw.put_slice(b"\r\nUser-Agent: runic-bench\r\n\r\n");

            let mut headers = [httparse::EMPTY_HEADER; 16];
            let mut req = httparse::Request::new(&mut headers);
            let Ok(httparse::Status::Complete(_)) = req.parse(&raw) else {
                continue;
            };

            let path = req.path.unwrap_or("");
            let mut header_map = HashMap::with_capacity(req.headers.len());
            for header in req.headers {
                let Ok(value) = std::str::from_utf8(header.value) else {
                    continue;
                };
                header_map.insert(header.name.to_ascii_lowercase(), value.to_owned());
            }

            let body = format!(
                "{{\"id\":{},\"round\":{},\"path\":\"{path}\",\"headers\":{}}}",
                i,
                round,
                header_map.len()
            );
            let mut resp = BytesMut::with_capacity(64 + body.len());
            resp.put_slice(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ",
            );
            resp.put_slice(body.len().to_string().as_bytes());
            resp.put_slice(b"\r\n\r\n");
            resp.put_slice(body.as_bytes());

            checksum ^= path.len() ^ header_map.len() ^ resp.len();
            if let Some(id) = header_map.get("x-request-id") {
                checksum ^= id.len();
            }
            black_box((header_map, resp.freeze()));
        }
    }
    black_box(checksum)
}
