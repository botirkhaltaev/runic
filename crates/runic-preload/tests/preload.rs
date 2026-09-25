//! Drives the fixture with `librunic.so` loaded ahead of libc.

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};

use object::{Object, ObjectSymbol};
use runic_preload::LIBRARY;

/// Everything `librunic.so` must interpose, and nothing more: an extra export
/// would replace an unrelated libc symbol in every preloaded process.
const EXPORTS: [&str; 21] = [
    "__libc_calloc",
    "__libc_cfree",
    "__libc_free",
    "__libc_malloc",
    "__libc_memalign",
    "__libc_posix_memalign",
    "__libc_pvalloc",
    "__libc_realloc",
    "__libc_valloc",
    "aligned_alloc",
    "calloc",
    "cfree",
    "free",
    "malloc",
    "malloc_usable_size",
    "memalign",
    "posix_memalign",
    "pvalloc",
    "realloc",
    "reallocarray",
    "valloc",
];

const RUNIC_ENV: [&str; 7] = [
    "RUNIC_MODE",
    "RUNIC_HUGEPAGE",
    "RUNIC_NUMA",
    "RUNIC_EXTENT_POLICY",
    "RUNIC_EXTENT_SLOTS",
    "RUNIC_EXTENT_BYTES",
    "RUNIC_RUN_POLICY",
];

fn command(case: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_preload-case"));
    command.arg(case).env("LD_PRELOAD", LIBRARY);
    for key in RUNIC_ENV {
        command.env_remove(key);
    }
    command
}

fn preloaded(case: &str) -> ExitStatus {
    command(case)
        .status()
        .expect("failed to run the preload fixture")
}

#[test]
fn malloc_comes_from_the_interceptor() {
    let status = preloaded("interposed");

    assert!(status.success(), "interposed case exited with {status}");
}

#[test]
fn threads_exit_while_bound() {
    let status = preloaded("threads");

    assert!(status.success(), "threads case exited with {status}");
}

#[test]
fn invalid_pointers_abort() {
    for case in [
        "unknown",
        "small-interior",
        "large-interior",
        "realloc-interior",
    ] {
        let status = preloaded(case);

        assert_eq!(
            status.signal(),
            Some(libc::SIGABRT),
            "{case} exited with {status} instead of aborting"
        );
    }
}

#[test]
fn unimplemented_modes_abort_at_init() {
    for mode in ["safe", "hardened"] {
        let status = command("interposed")
            .env("RUNIC_MODE", mode)
            .status()
            .expect("failed to run the preload fixture");

        assert_eq!(
            status.signal(),
            Some(libc::SIGABRT),
            "{mode} exited with {status} instead of aborting"
        );
    }
}

#[test]
fn fast_mode_runs() {
    let status = command("interposed")
        .env("RUNIC_MODE", "fast")
        .status()
        .expect("failed to run the preload fixture");

    assert!(status.success(), "fast exited with {status}");
}

#[test]
fn unknown_env_values_are_ignored() {
    let status = command("interposed")
        .env("RUNIC_MODE", "turbo")
        .env("RUNIC_HUGEPAGE", "huge")
        .env("RUNIC_NUMA", "bind")
        .env("RUNIC_EXTENT_SLOTS", "-1")
        .status()
        .expect("failed to run the preload fixture");

    assert!(status.success(), "unknown env values exited with {status}");
}

#[test]
fn exports_only_the_malloc_family() {
    let image = std::fs::read(LIBRARY).expect("failed to read librunic.so");
    let library = object::File::parse(&*image).expect("failed to parse librunic.so");

    let mut exports: Vec<_> = library
        .dynamic_symbols()
        .filter(object::ObjectSymbol::is_definition)
        .map(|symbol| symbol.name().expect("unnamed export").to_owned())
        .collect();
    exports.sort_unstable();

    assert_eq!(exports, EXPORTS);
}
