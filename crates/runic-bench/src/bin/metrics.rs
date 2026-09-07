use std::{env, process::Command};

use runic::{Budget, ExtentPolicy, RunicAlloc};
use runic_bench::{
    metrics::{Report, Syscalls},
    micro, programs,
    target::{self, AllocatorTarget, TARGETS},
};

static EXTENT_DROP: RunicAlloc = RunicAlloc::builder()
    .extent()
    .policy(ExtentPolicy::Drop)
    .budget(Budget::new(0, 0))
    .done()
    .build();
static EXTENT_TIGHT: RunicAlloc = RunicAlloc::builder()
    .extent()
    .policy(ExtentPolicy::Keep)
    .budget(Budget::new(2, 512 * 1024))
    .done()
    .build();

const EXTENT_TARGETS: &[AllocatorTarget] = &[
    AllocatorTarget::new("runic:extent_drop", &EXTENT_DROP),
    AllocatorTarget::new("runic:extent_tight", &EXTENT_TIGHT),
];

const CASES: &[&str] = &[
    "larson",
    "xmalloc",
    "cache_thrash",
    "cache_scratch",
    "sh6bench",
    "cfrac",
    "recycled_churn",
    "small_biased_random",
    "large_churn",
];

const PROGRAM_OPS: usize = 8_192;
const MICRO_OPS: usize = 10_000;
const LARGE_OPS: usize = 1_000;
const RANDOM_SEED: u64 = 0xf3ee_a110_c001_cafe;

fn main() {
    let args = env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|arg| arg == "--case") {
        run_case(&args);
        return;
    }

    let mut selected_targets = Vec::new();
    let mut selected_cases = Vec::new();
    let mut threads = 4_usize;
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
            "--threads" => {
                threads = args
                    .get(index + 1)
                    .expect("missing --threads value")
                    .parse()
                    .expect("invalid --threads");
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
        selected_cases = CASES.iter().map(|case| (*case).to_string()).collect();
    }

    Report::print_csv_header();
    for target in &selected_targets {
        for case in &selected_cases {
            run_subprocess(target, case, threads, syscalls);
        }
    }
}

fn run_subprocess(allocator: &str, workload: &str, threads: usize, syscalls: bool) {
    let exe = env::current_exe().unwrap();
    let case_args = [
        "--case".to_string(),
        allocator.to_string(),
        workload.to_string(),
        threads.to_string(),
    ];

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
            .output()
    } else {
        Command::new(exe).args(&case_args).output()
    };

    let output = match output {
        Ok(output) => output,
        Err(err) if syscalls => {
            eprintln!("warning: perf stat unavailable ({err}); retrying without syscalls");
            Command::new(env::current_exe().unwrap())
                .args(&case_args)
                .output()
                .unwrap()
        }
        Err(err) => panic!("metrics case failed to spawn: {err}"),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if syscalls && (stderr.contains("not supported") || stderr.contains("Permission")) {
            eprintln!("warning: perf syscall tracepoints not permitted; retrying without syscalls");
            let retry = Command::new(env::current_exe().unwrap())
                .args(&case_args)
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

fn resolve_target(name: &str) -> AllocatorTarget {
    target::by_name(name)
        .or_else(|| {
            EXTENT_TARGETS
                .iter()
                .copied()
                .find(|target| target.name() == name)
        })
        .unwrap_or_else(|| panic!("unknown allocator: {name}"))
}

fn run_case(args: &[String]) {
    let allocator = args.get(2).map(String::as_str).expect("missing allocator");
    let workload = args.get(3).map(String::as_str).expect("missing workload");
    let threads = args
        .get(4)
        .map_or(1, |value| value.parse().expect("invalid threads"));
    let target = resolve_target(allocator);

    match workload {
        "larson" => Report::measure(target.name(), "larson", threads, PROGRAM_OPS, || {
            let _ = programs::larson(target, threads, PROGRAM_OPS);
        }),
        "xmalloc" => Report::measure(target.name(), "xmalloc", threads, PROGRAM_OPS, || {
            let _ = programs::xmalloc(target, threads, PROGRAM_OPS);
        }),
        "cache_thrash" => {
            Report::measure(target.name(), "cache_thrash", threads, PROGRAM_OPS, || {
                let _ = programs::cache_thrash(target, threads, PROGRAM_OPS);
            })
        }
        "cache_scratch" => {
            Report::measure(target.name(), "cache_scratch", threads, PROGRAM_OPS, || {
                let _ = programs::cache_scratch(target, threads, PROGRAM_OPS);
            })
        }
        "sh6bench" => Report::measure(target.name(), "sh6bench", threads, PROGRAM_OPS, || {
            let _ = programs::sh6bench(target, threads, PROGRAM_OPS);
        }),
        "cfrac" => Report::measure(target.name(), "cfrac", threads, PROGRAM_OPS, || {
            let _ = programs::cfrac(target, threads, PROGRAM_OPS);
        }),
        "recycled_churn" => Report::measure(target.name(), "recycled_churn", 1, MICRO_OPS, || {
            let _ = micro::recycled_churn(target, 64, MICRO_OPS, 256);
        }),
        "small_biased_random" => {
            Report::measure(target.name(), "small_biased_random", 1, MICRO_OPS, || {
                let _ = micro::small_biased_random(target, RANDOM_SEED, MICRO_OPS, 1024);
            })
        }
        "large_churn" => Report::measure(target.name(), "large_churn", 1, LARGE_OPS, || {
            let _ = micro::large_churn(target, 256 * 1024, LARGE_OPS);
        }),
        _ => panic!("unknown workload: {workload}"),
    }
    .print_csv();
}
