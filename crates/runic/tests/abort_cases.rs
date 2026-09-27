//! Invalid operations end the process with `SIGABRT`. Each case runs in its
//! own `abort-case` process so the harness survives.

use std::os::unix::process::ExitStatusExt;
use std::process::Command;

fn run(case: &str) -> Option<i32> {
    Command::new(env!("CARGO_BIN_EXE_abort-case"))
        .arg(case)
        .status()
        .unwrap()
        .signal()
}

fn assert_aborts(case: &str) {
    assert_eq!(run(case), Some(libc::SIGABRT), "{case} did not abort");
}

#[test]
fn null_dealloc_aborts() {
    assert_aborts("null-free");
}

/// A pointer the allocator never issued, freed with a small layout, dies on
/// the run-header probe: an abort when that page is mapped, a fault when it
/// is not. Either way the process does not continue.
#[test]
fn unknown_pointer_free_is_fatal() {
    let signal = run("unknown-free");
    assert!(
        signal == Some(libc::SIGABRT) || signal == Some(libc::SIGSEGV),
        "unknown-free ended with {signal:?}"
    );
}

#[test]
fn interior_pointer_free_aborts() {
    assert_aborts("small-interior-free");
    assert_aborts("large-interior-free");
}

#[test]
fn interior_pointer_realloc_aborts() {
    assert_aborts("small-interior-realloc");
    assert_aborts("large-interior-realloc");
}

#[cfg(feature = "safe")]
#[test]
fn owner_double_free_aborts() {
    assert_aborts("small-double-free");
    assert_aborts("large-double-free");
}

#[cfg(feature = "safe")]
#[test]
fn remote_double_free_aborts() {
    assert_aborts("small-remote-double-free");
    assert_aborts("large-remote-double-free");
}
