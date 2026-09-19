use std::{
    hint::black_box,
    sync::{Arc, Mutex, mpsc},
    thread,
};

const THREADS: usize = 4;
const ROUNDS: usize = 4;
const JOBS: usize = 256;

pub(super) const ELEMENTS: usize = ROUNDS * JOBS;

/// Long-lived worker pool: the main thread submits jobs over a shared `mpsc`
/// queue, workers build `Vec<String>` results, and the main thread drops them,
/// so every result is freed by a thread that did not allocate it.
#[must_use]
pub(super) fn run() -> usize {
    const FIELDS: usize = 8;

    let (job_tx, job_rx) = mpsc::sync_channel::<(usize, usize)>(THREADS * 8);
    let (result_tx, result_rx) = mpsc::channel::<(usize, Vec<String>)>();
    let queue = Arc::new(Mutex::new(job_rx));
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let queue = Arc::clone(&queue);
            let result_tx = result_tx.clone();
            thread::spawn(move || {
                // The guard is released inside `map`, before the job runs.
                while let Ok(Ok((round, index))) = queue.lock().map(|inbox| inbox.recv()) {
                    let fields: Vec<String> = (0..FIELDS)
                        .map(|field| format!("r{round}-j{index}-f{field}"))
                        .collect();
                    let local = round ^ index ^ fields.iter().map(String::len).sum::<usize>();
                    if result_tx.send((local, fields)).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();
    drop(result_tx);

    let mut checksum = 0_usize;
    for round in 0..ROUNDS {
        for index in 0..JOBS {
            if job_tx.send((round, index)).is_err() {
                break;
            }
        }
        for _ in 0..JOBS {
            let Ok((local, fields)) = result_rx.recv() else {
                break;
            };
            checksum ^= local;
            black_box(fields);
        }
    }
    drop(job_tx);
    for worker in workers {
        let _ = worker.join();
    }
    black_box(checksum)
}
