use core::ptr::NonNull;
use std::sync::{Arc, Barrier};
use std::thread;

use rseq_rs::{CpuId, LockedStacks, Rseq};

fn token(n: usize) -> NonNull<u8> {
    NonNull::new(n as *mut u8).expect("nonzero")
}

#[test]
fn locked_quiesce_drains() {
    let stacks = LockedStacks::<u8>::new(2, 8).expect("map");
    let cpu = CpuId::new(0).expect("0");
    for i in 1..=4 {
        stacks.push_cpu(cpu, token(i)).expect("push");
    }
    let mut q = stacks.quiesce(cpu).expect("quiesce");
    let got: Vec<_> = q.drain().collect();
    drop(q);
    assert_eq!(got, [token(4), token(3), token(2), token(1)]);
    assert!(stacks.pop_cpu(cpu).is_none());
}

#[test]
fn rseq_quiesce_when_available() {
    let Some(rseq) = Rseq::try_new() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    let thread = rseq.bind().expect("bind");
    let stacks = rseq.stacks::<u8>(8).expect("stacks");
    stacks.push(&thread, token(1)).expect("push");
    stacks.push(&thread, token(2)).expect("push");
    let cpu = thread.cpu_id().expect("cpu");
    let mut q = stacks.quiesce(cpu).expect("quiesce");
    let got: Vec<_> = q.drain().collect();
    drop(q);
    assert_eq!(got.len(), 2);
    assert!(stacks.pop(&thread).is_none());
}

#[test]
fn locked_hitters_see_empty_during_quiesce() {
    let stacks = Arc::new(LockedStacks::<u8>::new(2, 32).expect("map"));
    let cpu = CpuId::new(0).expect("0");
    for i in 1..=16 {
        stacks.push_cpu(cpu, token(i)).expect("push");
    }
    let barrier = Arc::new(Barrier::new(2));
    let hitter = {
        let stacks = Arc::clone(&stacks);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            let mut n = 0;
            for _ in 0..64 {
                if stacks.pop_cpu(cpu).is_some() {
                    n += 1;
                }
            }
            n
        })
    };
    barrier.wait();
    let mut q = stacks.quiesce(cpu).expect("quiesce");
    let drained: Vec<_> = q.drain().collect();
    drop(q);
    let raced = hitter.join().expect("join");
    let mut seen = [false; 17];
    for p in drained.iter().copied() {
        let n = p.as_ptr() as usize;
        assert!((1..=16).contains(&n));
        assert!(!seen[n], "dup {n}");
        seen[n] = true;
    }
    assert_eq!(drained.len() + raced, 16);
}
