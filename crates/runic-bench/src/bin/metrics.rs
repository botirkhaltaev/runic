use std::{
    alloc::{GlobalAlloc, Layout, System},
    env,
    ffi::c_char,
    process::Command,
    sync::atomic::{AtomicU8, Ordering},
};

use mimalloc::MiMalloc;
use runic::{Budget, ExtentPolicy, RunPolicy, RunicAlloc};
use runic_bench::{
    metrics::{Report, Syscalls},
    target::{self, TARGETS},
    workloads::{self, WORKLOADS},
};
use snmalloc_rs::SnMalloc;
use tikv_jemallocator::Jemalloc;

static RUNIC: RunicAlloc = RunicAlloc::new();
static SYSTEM: System = System;
static MIMALLOC: MiMalloc = MiMalloc;
static JEMALLOC: Jemalloc = Jemalloc;
static SNMALLOC: SnMalloc = SnMalloc;
static KEEP_KEEP: RunicAlloc = RunicAlloc::builder()
    .extent_policy(ExtentPolicy::Keep)
    .run_policy(RunPolicy::Keep)
    .build();
static DISCARD_KEEP: RunicAlloc = RunicAlloc::builder()
    .extent_policy(ExtentPolicy::Discard)
    .run_policy(RunPolicy::Keep)
    .build();
static KEEP_DISCARD: RunicAlloc = RunicAlloc::builder()
    .extent_policy(ExtentPolicy::Keep)
    .run_policy(RunPolicy::Discard)
    .build();
static DISCARD_DISCARD: RunicAlloc = RunicAlloc::builder()
    .extent_policy(ExtentPolicy::Discard)
    .run_policy(RunPolicy::Discard)
    .build();
static UNMAP_KEEP: RunicAlloc = RunicAlloc::builder()
    .extent_policy(ExtentPolicy::Unmap)
    .run_policy(RunPolicy::Keep)
    .build();
static KEEP_KEEP_TIGHT: RunicAlloc = RunicAlloc::builder()
    .extent_policy(ExtentPolicy::Keep)
    .run_policy(RunPolicy::Keep)
    .extent_budget(Budget::new(2, 512 * 1024))
    .build();

const EXTRA_NAMES: &[&str] = &[
    "runic:keep/keep",
    "runic:discard/keep",
    "runic:keep/discard",
    "runic:discard/discard",
    "runic:unmap/keep",
    "runic:keep/keep:tight",
];

struct SelectedAlloc;

static KIND: AtomicU8 = AtomicU8::new(0);

fn kind_of(name: &[u8]) -> Option<u8> {
    Some(match name {
        b"runic" => 0,
        b"system" => 1,
        b"mimalloc" => 2,
        b"jemalloc" => 3,
        b"snmalloc" => 4,
        b"runic:keep/keep" => 5,
        b"runic:discard/keep" => 6,
        b"runic:keep/discard" => 7,
        b"runic:discard/discard" => 8,
        b"runic:unmap/keep" => 9,
        b"runic:keep/keep:tight" => 10,
        _ => return None,
    })
}

fn c_bytes(ptr: *const c_char) -> Option<&'static [u8]> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0_usize;
    // SAFETY: `getenv` returns a NUL-terminated C string or null.
    unsafe {
        while *ptr.add(len) != 0 {
            len = len.checked_add(1)?;
        }
        Some(core::slice::from_raw_parts(ptr.cast::<u8>(), len))
    }
}

unsafe extern "C" fn select_from_env() {
    // SAFETY: CRT has installed the environment; this runs before `main`.
    let ptr = unsafe { libc::getenv(c"RUNIC_BENCH_ALLOC".as_ptr()) };
    let Some(name) = c_bytes(ptr) else {
        return;
    };
    if let Some(kind) = kind_of(name) {
        KIND.store(kind, Ordering::Relaxed);
    }
}

#[used]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
static SELECT: unsafe extern "C" fn() = select_from_env;

fn selected() -> &'static dyn GlobalAlloc {
    match KIND.load(Ordering::Relaxed) {
        1 => &SYSTEM,
        2 => &MIMALLOC,
        3 => &JEMALLOC,
        4 => &SNMALLOC,
        5 => &KEEP_KEEP,
        6 => &DISCARD_KEEP,
        7 => &KEEP_DISCARD,
        8 => &DISCARD_DISCARD,
        9 => &UNMAP_KEEP,
        10 => &KEEP_KEEP_TIGHT,
        _ => &RUNIC,
    }
}

unsafe impl GlobalAlloc for SelectedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded to a live `GlobalAlloc`.
        unsafe { selected().alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded to a live `GlobalAlloc`.
        unsafe { selected().alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `alloc` on the same selected allocator.
        unsafe { selected().dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr` came from `alloc` on the same selected allocator.
        unsafe { selected().realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: SelectedAlloc = SelectedAlloc;

fn main() {
    let args = env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|arg| arg == "--case") {
        run_case(&args);
        return;
    }
    if args.get(1).is_some_and(|arg| arg == "--smoke") {
        smoke();
        return;
    }

    let mut selected_targets = Vec::new();
    let mut selected_cases = Vec::new();
    let mut syscalls = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--targets" => {
                let list = args.get(index + 1).expect("missing --targets value");
                selected_targets = list.split(',').map(str::trim).map(str::to_string).collect();
                index += 2;
            }
            "--cases" => {
                let list = args.get(index + 1).expect("missing --cases value");
                selected_cases = list.split(',').map(str::trim).map(str::to_string).collect();
                index += 2;
            }
            "--syscalls" => {
                syscalls = true;
                index += 1;
            }
            other => panic!("unknown argument: {other}"),
        }
    }

    if selected_targets.is_empty() {
        selected_targets = TARGETS
            .iter()
            .map(|target| target.name().to_string())
            .collect();
    }
    if selected_cases.is_empty() {
        selected_cases = WORKLOADS
            .iter()
            .map(|workload| workload.name().to_string())
            .collect();
    }

    Report::print_csv_header();
    for target in &selected_targets {
        for case in &selected_cases {
            run_subprocess(target, case, syscalls);
        }
    }
}

fn smoke() {
    let mut names: Vec<&str> = TARGETS.iter().map(|target| target.name()).collect();
    names.extend(EXTRA_NAMES.iter().copied());
    for name in names {
        let status = Command::new(env::current_exe().unwrap())
            .args(["--case", name, "vec_push_clear"])
            .env("RUNIC_BENCH_ALLOC", name)
            .status()
            .unwrap();
        assert!(status.success(), "smoke failed for {name}");
    }
}

fn run_subprocess(allocator: &str, workload: &str, syscalls: bool) {
    let exe = env::current_exe().unwrap();
    let case_args = [
        "--case".to_string(),
        allocator.to_string(),
        workload.to_string(),
    ];
    let alloc_env = ("RUNIC_BENCH_ALLOC", allocator);

    let output = if syscalls {
        Command::new("perf")
            .args([
                "stat",
                "-x,",
                "-e",
                "syscalls:sys_enter_mmap,syscalls:sys_enter_munmap,syscalls:sys_enter_mremap,syscalls:sys_enter_madvise,syscalls:sys_enter_brk",
            ])
            .arg(exe.as_os_str())
            .args(&case_args)
            .env(alloc_env.0, alloc_env.1)
            .output()
    } else {
        Command::new(exe)
            .args(&case_args)
            .env(alloc_env.0, alloc_env.1)
            .output()
    };

    let output = match output {
        Ok(output) => output,
        Err(err) if syscalls => {
            eprintln!("warning: perf stat unavailable ({err}); retrying without syscalls");
            Command::new(env::current_exe().unwrap())
                .args(&case_args)
                .env(alloc_env.0, alloc_env.1)
                .output()
                .unwrap()
        }
        Err(err) => panic!("metrics case failed to spawn: {err}"),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if syscalls
            && (stderr.contains("not supported")
                || stderr.to_ascii_lowercase().contains("permission"))
        {
            eprintln!("warning: perf syscall tracepoints not permitted; retrying without syscalls");
            let retry = Command::new(env::current_exe().unwrap())
                .args(&case_args)
                .env(alloc_env.0, alloc_env.1)
                .output()
                .unwrap();
            assert!(
                retry.status.success(),
                "metrics case failed for {allocator}/{workload}: {}",
                String::from_utf8_lossy(&retry.stderr)
            );
            print!("{}", String::from_utf8_lossy(&retry.stdout));
            return;
        }
        panic!(
            "metrics case failed for {allocator}/{workload}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut row = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if syscalls {
        if let Some(counts) = parse_perf_syscalls(&String::from_utf8_lossy(&output.stderr)) {
            row = merge_syscalls(&row, counts);
        } else {
            eprintln!("warning: could not parse perf syscall counts for {allocator}/{workload}");
        }
    }
    println!("{row}");
}

fn parse_perf_syscalls(stderr: &str) -> Option<Syscalls> {
    let mut counts = Syscalls::default();
    let mut any = false;
    for line in stderr.lines() {
        let mut fields = line.split(',');
        let value = fields.next()?.trim();
        if value == "<not supported>" || value == "<not counted>" || value.is_empty() {
            continue;
        }
        let _unit = fields.next()?;
        let event = fields.next()?.trim();
        let parsed: u64 = value.parse().ok()?;
        match event {
            "syscalls:sys_enter_mmap" => counts.mmap = Some(parsed),
            "syscalls:sys_enter_munmap" => counts.munmap = Some(parsed),
            "syscalls:sys_enter_mremap" => counts.mremap = Some(parsed),
            "syscalls:sys_enter_madvise" => counts.madvise = Some(parsed),
            "syscalls:sys_enter_brk" => counts.brk = Some(parsed),
            _ => continue,
        }
        any = true;
    }
    any.then_some(counts)
}

fn merge_syscalls(row: &str, counts: Syscalls) -> String {
    let mut cols: Vec<&str> = row.split(',').collect();
    if cols.len() < 16 {
        return row.to_string();
    }
    let mmap = counts.mmap.map(|n| n.to_string()).unwrap_or_default();
    let munmap = counts.munmap.map(|n| n.to_string()).unwrap_or_default();
    let mremap = counts.mremap.map(|n| n.to_string()).unwrap_or_default();
    let madvise = counts.madvise.map(|n| n.to_string()).unwrap_or_default();
    let brk = counts.brk.map(|n| n.to_string()).unwrap_or_default();
    cols[11] = mmap.as_str();
    cols[12] = munmap.as_str();
    cols[13] = mremap.as_str();
    cols[14] = madvise.as_str();
    cols[15] = brk.as_str();
    cols.join(",")
}

fn known_allocator(name: &str) -> bool {
    target::by_name(name).is_some() || EXTRA_NAMES.contains(&name)
}

fn run_case(args: &[String]) {
    let allocator = args.get(2).map(String::as_str).expect("missing allocator");
    let workload = args.get(3).map(String::as_str).expect("missing workload");
    assert!(known_allocator(allocator), "unknown allocator: {allocator}");
    let Some(case) = workloads::by_name(workload) else {
        panic!("unknown workload: {workload}");
    };
    Report::measure(
        allocator_name(allocator),
        case.name(),
        1,
        case.elems(),
        || {
            let _ = case.run();
        },
    )
    .print_csv();
}

fn allocator_name(name: &str) -> &'static str {
    TARGETS
        .iter()
        .map(|target| target.name())
        .chain(EXTRA_NAMES.iter().copied())
        .find(|&known| known == name)
        .unwrap_or_else(|| panic!("unknown allocator: {name}"))
}

#[cfg(test)]
mod tests {
    use super::kind_of;

    #[test]
    fn kind_of_known_names() {
        assert_eq!(kind_of(b"runic"), Some(0));
        assert_eq!(kind_of(b"snmalloc"), Some(4));
        assert_eq!(kind_of(b"runic:keep/discard"), Some(7));
        assert_eq!(kind_of(b"runic:discard/keep"), Some(6));
        assert_eq!(kind_of(b"runic:unmap/keep"), Some(9));
        assert_eq!(kind_of(b"nope"), None);
    }
}
