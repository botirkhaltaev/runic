use std::hint::black_box;

use httparse::{EMPTY_HEADER, Request, Status};
use tokio::{runtime::Builder, sync::mpsc as tokio_mpsc};

const THREADS: usize = 4;
const ROUNDS: usize = 4;
const REQUESTS: usize = 256;

pub(super) const ELEMENTS: usize = ROUNDS * REQUESTS;

/// Async HTTP service on a 4-worker Tokio runtime: connection tasks format
/// requests, parse them with `httparse`, and send responses over a
/// `tokio::sync::mpsc` channel to a collector that drops them.
///
/// Every task is awaited before the round ends, so the runtime shuts down idle.
#[must_use]
pub(super) fn run() -> usize {
    const CONNECTIONS: usize = THREADS;
    const MAX_HEADERS: usize = 8;
    const QUEUE: usize = 64;

    let Ok(runtime) = Builder::new_multi_thread().worker_threads(THREADS).build() else {
        return 0;
    };
    let checksum = runtime.block_on(async move {
        let mut checksum = 0_usize;
        for round in 0..ROUNDS {
            let (tx, mut rx) = tokio_mpsc::channel::<(usize, String)>(QUEUE);
            let connections: Vec<_> = (0..CONNECTIONS)
                .map(|conn| {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let mut served = 0_usize;
                        for i in 0..REQUESTS {
                            let raw = format!(
                                "GET /item/{i}?conn={conn} HTTP/1.1\r\n\
                                 host: bench.local\r\n\
                                 user-agent: runic/{round}\r\n\
                                 accept: */*\r\n\r\n"
                            );
                            let mut storage = [EMPTY_HEADER; MAX_HEADERS];
                            let mut request = Request::new(&mut storage);
                            let Ok(Status::Complete(head)) = request.parse(raw.as_bytes()) else {
                                continue;
                            };
                            let path = request.path.unwrap_or("/");
                            let host = request
                                .headers
                                .iter()
                                .find(|header| header.name.eq_ignore_ascii_case("host"))
                                .and_then(|header| std::str::from_utf8(header.value).ok())
                                .unwrap_or("bench.local");
                            let body =
                                format!("{{\"host\":\"{host}\",\"path\":\"{path}\",\"seq\":{i}}}");
                            let response = format!(
                                "HTTP/1.1 200 OK\r\n\
                                 content-type: application/json\r\n\
                                 content-length: {}\r\n\r\n{body}",
                                body.len()
                            );
                            if tx.send((head, response)).await.is_err() {
                                break;
                            }
                            served += 1;
                        }
                        served
                    })
                })
                .collect();
            drop(tx);
            while let Some((head, response)) = rx.recv().await {
                checksum ^= head ^ response.len();
                black_box(response);
            }
            for connection in connections {
                if let Ok(served) = connection.await {
                    checksum ^= served;
                }
            }
        }
        checksum
    });
    black_box(checksum)
}
