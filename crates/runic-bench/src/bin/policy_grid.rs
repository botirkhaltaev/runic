use std::{alloc::GlobalAlloc, env, process::Command};

use runic::{Budget, ExtentPolicy, ExtentReuse, RunPolicy, RunicAlloc};
use runic_bench::{allocator_target::AllocatorTarget, rss::RssReport, workload};

static EXTENT_DROP: RunicAlloc = RunicAlloc::builder()
    .extent()
    .policy(ExtentPolicy::Drop)
    .budget(Budget::new(0, 0))
    .done()
    .build();
static EXTENT_KEEP_16M: RunicAlloc = RunicAlloc::new();
static EXTENT_FIFO_16M: RunicAlloc = RunicAlloc::builder()
    .extent()
    .policy(ExtentPolicy::Fifo)
    .budget(Budget::new(32, 16 * 1024 * 1024))
    .done()
    .build();
static EXTENT_LIFO_16M: RunicAlloc = RunicAlloc::builder()
    .extent()
    .policy(ExtentPolicy::Lifo)
    .budget(Budget::new(32, 16 * 1024 * 1024))
    .done()
    .build();
static EXTENT_LARGEST_16M: RunicAlloc = RunicAlloc::builder()
    .extent()
    .policy(ExtentPolicy::Largest)
    .budget(Budget::new(32, 16 * 1024 * 1024))
    .done()
    .build();
static EXTENT_SMALLEST_16M: RunicAlloc = RunicAlloc::builder()
    .extent()
    .policy(ExtentPolicy::Smallest)
    .budget(Budget::new(32, 16 * 1024 * 1024))
    .done()
    .build();
static EXTENT_FIFO_BEST_FIT_16M: RunicAlloc = RunicAlloc::builder()
    .extent()
    .policy(ExtentPolicy::Fifo)
    .reuse(ExtentReuse::BestFit)
    .budget(Budget::new(32, 16 * 1024 * 1024))
    .done()
    .build();
static EXTENT_FIFO_SIZE_CLASS_16M: RunicAlloc = RunicAlloc::builder()
    .extent()
    .policy(ExtentPolicy::Fifo)
    .reuse(ExtentReuse::SizeClass)
    .budget(Budget::new(32, 16 * 1024 * 1024))
    .done()
    .build();
static RUN_DROP_EMPTY: RunicAlloc = RunicAlloc::builder()
    .run()
    .policy(RunPolicy::DropEmpty)
    .done()
    .build();
static RUN_RETAIN_FIFO_512K: RunicAlloc = RunicAlloc::builder()
    .run()
    .policy(RunPolicy::RetainFifo)
    .budget(Budget::new(8, 512 * 1024))
    .done()
    .build();
static RUN_RETAIN_PER_CLASS_512K: RunicAlloc = RunicAlloc::builder()
    .run()
    .policy(RunPolicy::RetainPerClass)
    .budget(Budget::new(8, 512 * 1024))
    .done()
    .build();

const CONFIGS: &[PolicyConfig] = &[
    PolicyConfig::new("extent_drop", &EXTENT_DROP),
    PolicyConfig::new("extent_keep_16m", &EXTENT_KEEP_16M),
    PolicyConfig::new("extent_fifo_16m", &EXTENT_FIFO_16M),
    PolicyConfig::new("extent_lifo_16m", &EXTENT_LIFO_16M),
    PolicyConfig::new("extent_largest_16m", &EXTENT_LARGEST_16M),
    PolicyConfig::new("extent_smallest_16m", &EXTENT_SMALLEST_16M),
    PolicyConfig::new("extent_fifo_best_fit_16m", &EXTENT_FIFO_BEST_FIT_16M),
    PolicyConfig::new("extent_fifo_size_class_16m", &EXTENT_FIFO_SIZE_CLASS_16M),
    PolicyConfig::new("run_drop_empty", &RUN_DROP_EMPTY),
    PolicyConfig::new("run_retain_fifo_512k", &RUN_RETAIN_FIFO_512K),
    PolicyConfig::new("run_retain_per_class_512k", &RUN_RETAIN_PER_CLASS_512K),
];

const WORKLOADS: &[PolicyWorkload] = &[
    PolicyWorkload {
        name: "large_alloc_churn_256k",
        ops: 1_000,
    },
    PolicyWorkload {
        name: "mixed_large_churn",
        ops: 1_000,
    },
    PolicyWorkload {
        name: "single_size_churn_64",
        ops: 10_000,
    },
    PolicyWorkload {
        name: "small_biased_random",
        ops: 20_000,
    },
    PolicyWorkload {
        name: "run_empty_churn",
        ops: 4,
    },
];

#[derive(Clone, Copy)]
struct PolicyConfig {
    name: &'static str,
    allocator: &'static (dyn GlobalAlloc + Sync),
}

impl PolicyConfig {
    const fn new(name: &'static str, allocator: &'static (dyn GlobalAlloc + Sync)) -> Self {
        Self { name, allocator }
    }

    const fn target(self) -> AllocatorTarget {
        AllocatorTarget::new(self.name, self.allocator)
    }
}

#[derive(Clone, Copy)]
struct PolicyWorkload {
    name: &'static str,
    ops: usize,
}

fn main() {
    let args = env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|arg| arg == "--case") {
        run_case(&args);
        return;
    }

    RssReport::print_csv_header();

    for config in CONFIGS {
        for workload in WORKLOADS {
            run_subprocess(config.name, workload.name);
        }
    }
}

fn run_subprocess(config: &str, workload: &str) {
    let exe = env::current_exe().unwrap();
    let output = Command::new(exe)
        .args(["--case", config, workload])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "policy grid case failed for {config}/{workload}: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    print!("{}", String::from_utf8_lossy(&output.stdout));
}

fn run_case(args: &[String]) {
    let config_name = args.get(2).map(String::as_str).expect("missing config");
    let workload_name = args.get(3).map(String::as_str).expect("missing workload");
    let config = CONFIGS
        .iter()
        .copied()
        .find(|config| config.name == config_name)
        .expect("unknown config");
    let workload = WORKLOADS
        .iter()
        .copied()
        .find(|workload| workload.name == workload_name)
        .expect("unknown workload");
    let ops = args.get(4).map_or(workload.ops, |value| {
        value.parse::<usize>().expect("invalid ops")
    });
    let target = config.target();

    match workload.name {
        "large_alloc_churn_256k" => RssReport::measure(config.name, workload.name, ops, || {
            let _checksum = workload::large_alloc_churn(target, 256 * 1024, ops);
        }),
        "mixed_large_churn" => RssReport::measure(config.name, workload.name, ops, || {
            let _checksum = workload::mixed_large_churn(target, ops);
        }),
        "single_size_churn_64" => RssReport::measure(config.name, workload.name, ops, || {
            let _checksum = workload::single_size_churn(target, 64, ops);
        }),
        "small_biased_random" => RssReport::measure(config.name, workload.name, ops, || {
            let _checksum = workload::small_biased_random(target, 0xf3ee_a110_c001_cafe, ops, 1024);
        }),
        "run_empty_churn" => RssReport::measure(config.name, workload.name, ops, || {
            let _checksum = workload::run_empty_churn(target, ops);
        }),
        _ => unreachable!(),
    }
    .print_csv();
}
