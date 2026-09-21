use std::{
    hint::black_box,
    sync::{Arc, Mutex, PoisonError, mpsc},
    thread,
};

const THREADS: usize = 4;
const ROUNDS: usize = 4;
const BUFFERS: usize = 32;
const JOBS: usize = 128;
const BUFFER: usize = 64 * 1024;

pub(super) const ELEMENTS: usize = ROUNDS * JOBS;

/// Shared 64 KiB `Vec<u8>` pool: workers check out a buffer, fill it, and
/// return it. Retention and reuse of large blocks is the allocator lever.
#[must_use]
pub(super) fn run() -> usize {
    let pool = Arc::new(Mutex::new(
        (0..BUFFERS)
            .map(|_| vec![0_u8; BUFFER])
            .collect::<Vec<Vec<u8>>>(),
    ));
    let mut checksum = 0_usize;
    for round in 0..ROUNDS {
        let (job_tx, job_rx) = mpsc::sync_channel::<usize>(THREADS * 8);
        let (done_tx, done_rx) = mpsc::channel::<usize>();
        let queue = Arc::new(Mutex::new(job_rx));
        thread::scope(|scope| {
            for worker in 0..THREADS {
                let inbox = Arc::clone(&queue);
                let buffers = Arc::clone(&pool);
                let outbound = done_tx.clone();
                scope.spawn(move || {
                    while let Ok(Ok(job)) = inbox.lock().map(|guard| guard.recv()) {
                        let mut buffer = {
                            let mut free = buffers.lock().unwrap_or_else(PoisonError::into_inner);
                            free.pop().unwrap_or_else(|| vec![0_u8; BUFFER])
                        };
                        let fill = u8::try_from((job + worker + round) & 0xff).unwrap_or(0);
                        buffer.fill(fill);
                        let local = usize::from(buffer[0]) ^ usize::from(buffer[BUFFER - 1]) ^ job;
                        {
                            let mut free = buffers.lock().unwrap_or_else(PoisonError::into_inner);
                            free.push(buffer);
                        }
                        if outbound.send(local).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(done_tx);
            for job in 0..JOBS {
                if job_tx.send(job).is_err() {
                    break;
                }
            }
            drop(job_tx);
            while let Ok(local) = done_rx.recv() {
                checksum ^= local;
            }
        });
    }
    black_box(checksum)
}
