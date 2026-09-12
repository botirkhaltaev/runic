use core::ptr::NonNull;

use rseq_rs::{CpuId, LockedStacks};

fn token(n: usize) -> NonNull<u8> {
    NonNull::new(n as *mut u8).expect("nonzero fake pointer")
}

fn cpu0() -> CpuId {
    CpuId::new(0).expect("0 is not the uninit sentinel")
}

#[test]
fn pop_push_empty_full() {
    let stacks = LockedStacks::<u8>::new(2, 2).expect("map");
    assert!(stacks.pop_cpu(cpu0()).is_none());
    stacks.push_cpu(cpu0(), token(1)).expect("first");
    stacks.push_cpu(cpu0(), token(2)).expect("second");
    let full = stacks.push_cpu(cpu0(), token(3)).expect_err("full");
    assert_eq!(full.item(), token(3));
    assert_eq!(stacks.pop_cpu(cpu0()), Some(token(2)));
    assert_eq!(stacks.pop_cpu(cpu0()), Some(token(1)));
    assert!(stacks.pop_cpu(cpu0()).is_none());
}

#[test]
fn batch_roundtrip() {
    let stacks = LockedStacks::<u8>::new(2, 4).expect("map");
    let items = [token(1), token(2), token(3)];
    assert_eq!(stacks.push_batch_cpu(cpu0(), &items), 3);
    let mut out = [token(99); 4];
    assert_eq!(stacks.pop_batch_cpu(cpu0(), &mut out), 3);
    assert_eq!(&out[..3], &[token(3), token(2), token(1)]);
}

#[test]
fn unique_tokens() {
    let stacks = LockedStacks::<u8>::new(2, 8).expect("map");
    for i in 1..=8 {
        stacks.push_cpu(cpu0(), token(i)).expect("push");
    }
    let mut seen = [false; 9];
    for _ in 0..8 {
        let p = stacks.pop_cpu(cpu0()).expect("pop");
        let n = p.as_ptr() as usize;
        assert!((1..=8).contains(&n));
        assert!(!seen[n], "duplicate {n}");
        seen[n] = true;
    }
    assert!(seen[1..].iter().all(|s| *s));
}

#[test]
fn isolated_cpus() {
    let stacks = LockedStacks::<u8>::new(2, 2).expect("map");
    let cpu1 = CpuId::new(1).expect("1");
    stacks.push_cpu(cpu0(), token(1)).expect("cpu0");
    stacks.push_cpu(cpu1, token(2)).expect("cpu1");
    assert_eq!(stacks.pop_cpu(cpu0()), Some(token(1)));
    assert_eq!(stacks.pop_cpu(cpu1), Some(token(2)));
}
