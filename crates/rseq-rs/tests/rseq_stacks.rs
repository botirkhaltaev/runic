use core::ptr::NonNull;

use rseq_rs::{Rseq, Stacks};

fn token(n: usize) -> NonNull<u8> {
    NonNull::new(n as *mut u8).expect("nonzero fake pointer")
}

fn ready() -> Option<(Rseq, rseq_rs::Thread, Stacks<u8>)> {
    let rseq = Rseq::try_new()?;
    let thread = rseq.bind()?;
    let stacks = rseq.stacks::<u8>(4)?;
    Some((rseq, thread, stacks))
}

#[test]
fn pop_push_when_available() {
    let Some((_rseq, thread, stacks)) = ready() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    assert!(stacks.pop(&thread).is_none());
    stacks.push(&thread, token(1)).expect("push");
    stacks.push(&thread, token(2)).expect("push");
    assert_eq!(stacks.pop(&thread), Some(token(2)));
    assert_eq!(stacks.pop(&thread), Some(token(1)));
    assert!(stacks.pop(&thread).is_none());
}

#[test]
fn batch_when_available() {
    let Some((_rseq, thread, stacks)) = ready() else {
        eprintln!("skip: rseq unavailable");
        return;
    };
    let items = [token(1), token(2)];
    assert_eq!(stacks.push_batch(&thread, &items), 2);
    let mut out = [token(99); 2];
    assert_eq!(stacks.pop_batch(&thread, &mut out), 2);
    assert_eq!(out, [token(2), token(1)]);
}
