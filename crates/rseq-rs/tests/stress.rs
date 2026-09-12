//! Abort-heavy uniqueness. `cargo test -p rseq-rs -- --ignored`.

use std::mem::size_of;
use std::os::raw::c_int;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use rseq_rs::{Rseq, Words};

fn pin(cpu: usize) {
    unsafe {
        let mut set = std::mem::zeroed::<libc::cpu_set_t>();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set);
    }
}

extern "C" fn ignore_alrm(_sig: c_int) {}

fn start_alrm() {
    unsafe {
        libc::signal(
            libc::SIGALRM,
            ignore_alrm as *const () as libc::sighandler_t,
        );
        let mut it = std::mem::zeroed::<libc::itimerval>();
        it.it_interval.tv_usec = 200;
        it.it_value.tv_usec = 200;
        libc::setitimer(libc::ITIMER_REAL, &raw const it, std::ptr::null_mut());
    }
}

fn stop_alrm() {
    unsafe {
        let it = std::mem::zeroed::<libc::itimerval>();
        libc::setitimer(libc::ITIMER_REAL, &raw const it, std::ptr::null_mut());
    }
}

fn sum_words(rseq: Rseq, words: &Words) -> usize {
    let ncpus = usize::try_from(rseq.cpus()).expect("cpus");
    let mut sum = 0;
    for cpu in 0..ncpus {
        pin(cpu);
        let thread = rseq.bind().expect("bind");
        let id = thread.cpu_id().expect("cpu");
        let w = words.get(id).expect("word");
        sum += thread.fetch_add(w, 0);
    }
    sum
}

#[ignore = "affinity flap and SIGALRM; run with --ignored"]
#[test]
fn unique_add_under_migration_and_signals() {
    let Some(rseq) = Rseq::try_new() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    let words = Arc::new(rseq.words().expect("words"));
    let ncpus = usize::try_from(rseq.cpus()).expect("cpus");
    let nthreads = 8.min(ncpus.saturating_mul(2).max(2));
    let added = Arc::new(AtomicUsize::new(0));
    start_alrm();
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut joins = Vec::new();
    for _ in 0..nthreads {
        let words = Arc::clone(&words);
        let added = Arc::clone(&added);
        joins.push(thread::spawn(move || {
            let thread = rseq.bind().expect("bind");
            let mut i = 0usize;
            while Instant::now() < deadline {
                pin(i % ncpus);
                let Some(cpu) = thread.cpu_id() else {
                    i += 1;
                    continue;
                };
                let Some(w) = words.get(cpu) else {
                    i += 1;
                    continue;
                };
                let _ = thread.fetch_add(w, 1);
                added.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        }));
    }
    for j in joins {
        j.join().expect("join");
    }
    stop_alrm();
    assert_eq!(sum_words(rseq, &words), added.load(Ordering::Relaxed));
}

#[ignore = "affinity flap and SIGALRM; run with --ignored"]
#[test]
fn no_lost_cas_under_migration_and_signals() {
    let Some(rseq) = Rseq::try_new() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    let words = Arc::new(rseq.words().expect("words"));
    let ncpus = usize::try_from(rseq.cpus()).expect("cpus");
    let nthreads = 8.min(ncpus.saturating_mul(2).max(2));
    let won = Arc::new(AtomicUsize::new(0));
    start_alrm();
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut joins = Vec::new();
    for _ in 0..nthreads {
        let words = Arc::clone(&words);
        let won = Arc::clone(&won);
        joins.push(thread::spawn(move || {
            let thread = rseq.bind().expect("bind");
            let mut i = 0usize;
            let mut expect = 0usize;
            while Instant::now() < deadline {
                pin(i % ncpus);
                let Some(cpu) = thread.cpu_id() else {
                    i += 1;
                    continue;
                };
                let Some(w) = words.get(cpu) else {
                    i += 1;
                    continue;
                };
                match thread.compare_exchange(w, expect, expect.wrapping_add(1)) {
                    Ok(_) => {
                        won.fetch_add(1, Ordering::Relaxed);
                        expect = expect.wrapping_add(1);
                    }
                    Err(current) => expect = current,
                }
                i += 1;
            }
        }));
    }
    for j in joins {
        j.join().expect("join");
    }
    stop_alrm();
    assert_eq!(sum_words(rseq, &words), won.load(Ordering::Relaxed));
}
