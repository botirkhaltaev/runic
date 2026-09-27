//! Edges of the OS contract. The mmap case runs in a child so the address
//! limit does not poison the rest of the suite.

use std::alloc::Layout;
use std::env;
use std::process::Command;

use runic_core::Allocator;

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

/// `VmSize` in bytes, the virtual size a new mapping has to fit under.
fn vm_size() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmSize:"))
        .unwrap();
    let kb: usize = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    kb * 1024
}

fn mmap_limited() {
    let allocator = Allocator::new();
    let small = layout(64, 8);
    // SAFETY: the layout is valid and the block is freed before the limit.
    let warm = unsafe { allocator.alloc(small) };
    assert!(!warm.is_null());
    unsafe { allocator.dealloc(warm, small) };

    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_AS, &raw mut limit) },
        0
    );
    limit.rlim_cur = libc::rlim_t::try_from(vm_size()).unwrap();
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_AS, &raw const limit) },
        0
    );

    let huge = layout(64 * 1024 * 1024, 4096);
    // SAFETY: the layout is valid. The mapping cannot fit, so this returns null.
    assert!(unsafe { allocator.alloc(huge) }.is_null());

    // SAFETY: the run mapped before the limit is still there to reuse.
    let again = unsafe { allocator.alloc(small) };
    assert!(!again.is_null());
    unsafe {
        again.write(0x11);
        assert_eq!(again.read(), 0x11);
        allocator.dealloc(again, small);
    }
}

#[test]
fn failed_mmap_returns_null_and_leaves_the_allocator_usable() {
    if env::var_os("RUNIC_MMAP_LIMIT").is_some() {
        mmap_limited();
        return;
    }

    let status = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "failed_mmap_returns_null_and_leaves_the_allocator_usable",
            "--test-threads=1",
        ])
        .env("RUNIC_MMAP_LIMIT", "1")
        .status()
        .unwrap();

    assert!(status.success(), "child exited with {status}");
}

/// Fork with no allocation in progress. The child sees the parent's bytes,
/// allocates and frees its own block, and the parent still owns its block.
/// Fork from another thread, or during `alloc` or `free`, is unsupported
/// until `pthread_atfork`.
#[test]
fn quiescent_fork_keeps_the_parent_block_and_serves_the_child() {
    let allocator = Allocator::new();
    let block = layout(64, 8);
    // SAFETY: the layout is valid. Parent and child each free their own copy.
    let ptr = unsafe { allocator.alloc(block) };
    assert!(!ptr.is_null());
    unsafe { ptr.write_bytes(0x5a, block.size()) };

    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        unsafe {
            assert_eq!(ptr.read(), 0x5a);
            let child = allocator.alloc(block);
            assert!(!child.is_null());
            child.write(0x11);
            allocator.dealloc(child, block);
            allocator.dealloc(ptr, block);
        }
        unsafe { libc::_exit(0) };
    }

    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "child status {status}"
    );
    unsafe {
        assert_eq!(ptr.read(), 0x5a);
        allocator.dealloc(ptr, block);
    }
}
