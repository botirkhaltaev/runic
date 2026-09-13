use core::ptr::NonNull;

use rseq_rs::{CpuId, Word, Words};

fn cpu(id: u32) -> CpuId {
    CpuId::new(id).expect("not the uninit sentinel")
}

#[test]
fn new_sizes() {
    assert!(Words::new(0).is_none());
    let words = Words::new(2).expect("map");
    assert_eq!(words.cpus(), 2);
    let w = words.get(cpu(0)).expect("cpu 0");
    assert_eq!(w.cpu(), cpu(0));
    assert!(words.get(cpu(2)).is_none());
}

#[test]
fn from_raw_sizes() {
    let len = 4096usize;
    // SAFETY: anonymous private page.
    let ptr = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(ptr, libc::MAP_FAILED);
    let base = NonNull::new(ptr.cast()).expect("non-null mmap");
    // SAFETY: `base` is a live page we own until after `Words` drops.
    let words = unsafe { Words::from_raw(base, 2) };
    assert_eq!(words.cpus(), 2);
    let w = words.get(cpu(1)).expect("cpu 1");
    assert_eq!(w.cpu(), cpu(1));
    // SAFETY: slot is inside the mapping.
    let raw = unsafe { Word::from_raw(w.as_ptr(), cpu(1)) };
    assert_eq!(raw.cpu(), cpu(1));
    drop(words);
    // SAFETY: mapping is unused after `Words` dropped.
    unsafe {
        libc::munmap(ptr, len);
    }
}
