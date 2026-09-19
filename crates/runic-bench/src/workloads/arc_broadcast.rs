use std::{hint::black_box, sync::Arc, sync::mpsc, thread};

const THREADS: usize = 4;
const ROUNDS: usize = 4;
const MESSAGES: usize = 256;
const PAYLOAD: usize = 64;

pub(super) const ELEMENTS: usize = ROUNDS * MESSAGES;

/// Publisher allocates `Arc<str>` payloads, clones them to subscribers, then
/// drops the original so the last drop is on a thread that did not allocate.
#[must_use]
pub(super) fn run() -> usize {
    let mut checksum = 0_usize;
    for round in 0..ROUNDS {
        let mut senders = Vec::with_capacity(THREADS);
        let mut receivers = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let (tx, rx) = mpsc::sync_channel::<Arc<str>>(THREADS * 8);
            senders.push(tx);
            receivers.push(rx);
        }
        thread::scope(|scope| {
            for (worker, rx) in receivers.into_iter().enumerate() {
                scope.spawn(move || {
                    while let Ok(payload) = rx.recv() {
                        black_box(payload.len() ^ worker ^ usize::from(payload.as_bytes()[0]));
                        black_box(payload);
                    }
                });
            }
            for i in 0..MESSAGES {
                let mut body = format!("msg-{round}-{i}");
                body.reserve(PAYLOAD.saturating_sub(body.len()));
                while body.len() < PAYLOAD {
                    let offset = u8::try_from((i + round) % 26).unwrap_or(0);
                    body.push(char::from(b'a' + offset));
                }
                let payload: Arc<str> = Arc::from(body);
                checksum ^= payload.len();
                for tx in &senders {
                    if tx.send(Arc::clone(&payload)).is_err() {
                        return;
                    }
                }
                drop(payload);
            }
            drop(senders);
        });
    }
    black_box(checksum)
}
