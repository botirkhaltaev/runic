//! Abort-heavy uniqueness. `cargo test -p rseq-rs -- --ignored`.

use core::ptr::NonNull;
use std::mem::size_of;
use std::os::raw::c_int;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use rseq_rs::Rseq;

fn token(n: usize) -> NonNull<u8> {
    NonNull::new(n as *mut u8).expect("nonzero")
}

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

#[ignore = "affinity flap and SIGALRM; run with --ignored"]
#[test]
fn unique_under_migration_and_signals() {
    let Some(rseq) = Rseq::try_new() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    let stacks = Arc::new(rseq.stacks::<u8>(64).expect("stacks"));
    let ncpus = usize::try_from(rseq.cpus()).expect("cpus");
    let nthreads = 8.min(ncpus.saturating_mul(2).max(2));
    let pushed = Arc::new(AtomicUsize::new(0));
    let popped = Arc::new(Mutex::new(Vec::new()));
    start_alrm();
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut joins = Vec::new();
    for t in 0..nthreads {
        let stacks = Arc::clone(&stacks);
        let pushed = Arc::clone(&pushed);
        let popped = Arc::clone(&popped);
        joins.push(thread::spawn(move || {
            let thread = rseq.bind().expect("bind");
            let base = (t + 1) * 1_000_000;
            let mut i = 1usize;
            let mut local = Vec::new();
            while Instant::now() < deadline {
                pin(i % ncpus);
                let p = token(base + i);
                if stacks.push(&thread, p).is_ok() {
                    pushed.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(q) = stacks.pop(&thread) {
                    local.push(q.as_ptr() as usize);
                }
                i += 1;
            }
            popped.lock().expect("lock").extend(local);
        }));
    }
    for j in joins {
        j.join().expect("join");
    }
    stop_alrm();
    // Drain leftovers on this thread.
    let thread = rseq.bind().expect("bind");
    let mut rest = Vec::new();
    while let Some(p) = stacks.pop(&thread) {
        rest.push(p.as_ptr() as usize);
    }
    let mut all = popped.lock().expect("lock");
    all.extend(rest);
    all.sort_unstable();
    let n_pop = all.len();
    all.dedup();
    assert_eq!(all.len(), n_pop, "duplicate pop");
    assert_eq!(n_pop, pushed.load(Ordering::Relaxed), "loss");
}
