use std::{
    fs,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub struct RssSample {
    pub current_bytes: usize,
    pub peak_bytes: usize,
}

#[derive(Clone, Copy, Default)]
pub struct Syscalls {
    pub mmap: Option<u64>,
    pub munmap: Option<u64>,
    pub mremap: Option<u64>,
    pub madvise: Option<u64>,
    pub brk: Option<u64>,
}

pub struct Report {
    pub allocator: &'static str,
    pub workload: &'static str,
    pub threads: usize,
    pub ops: usize,
    pub elapsed: Duration,
    pub rss_before: usize,
    pub rss_peak: usize,
    pub rss_after_free: usize,
    pub vma_before: usize,
    pub vma_after: usize,
    pub minflt: u64,
    pub syscalls: Syscalls,
}

impl Report {
    #[must_use]
    pub fn measure(
        allocator: &'static str,
        workload: &'static str,
        threads: usize,
        ops: usize,
        run: impl FnOnce(),
    ) -> Self {
        let before = RssSample::read();
        let vma_before = vma_count();
        let faults_before = page_faults();
        let started = Instant::now();
        run();
        let elapsed = started.elapsed();
        let after = RssSample::read();
        let vma_after = vma_count();
        let faults_after = page_faults();

        Self {
            allocator,
            workload,
            threads,
            ops,
            elapsed,
            rss_before: before.current_bytes,
            rss_peak: after.peak_bytes.max(before.peak_bytes),
            rss_after_free: after.current_bytes,
            vma_before,
            vma_after,
            minflt: faults_after.minflt.saturating_sub(faults_before.minflt),
            syscalls: Syscalls::default(),
        }
    }

    pub fn print_csv_header() {
        println!(
            "allocator,workload,threads,ops,elapsed_ns,rss_before_bytes,rss_peak_bytes,rss_after_free_bytes,vma_before,vma_after,minflt,mmap,munmap,mremap,madvise,brk"
        );
    }

    pub fn print_csv(&self) {
        println!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            self.allocator,
            self.workload,
            self.threads,
            self.ops,
            self.elapsed.as_nanos(),
            self.rss_before,
            self.rss_peak,
            self.rss_after_free,
            self.vma_before,
            self.vma_after,
            self.minflt,
            opt(self.syscalls.mmap),
            opt(self.syscalls.munmap),
            opt(self.syscalls.mremap),
            opt(self.syscalls.madvise),
            opt(self.syscalls.brk),
        );
    }
}

fn opt(value: Option<u64>) -> String {
    value.map_or_else(String::new, |n| n.to_string())
}

impl RssSample {
    #[must_use]
    pub fn read() -> Self {
        let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
        let current_bytes = status_value_kb(&status, "VmRSS:").unwrap_or(0) * 1024;
        let peak_bytes = status_value_kb(&status, "VmHWM:").unwrap_or(current_bytes / 1024) * 1024;
        Self {
            current_bytes,
            peak_bytes,
        }
    }
}

#[must_use]
pub fn vma_count() -> usize {
    fs::read_to_string("/proc/self/maps").map_or(0, |maps| maps.lines().count())
}

struct Faults {
    minflt: u64,
}

fn page_faults() -> Faults {
    let stat = fs::read_to_string("/proc/self/stat").unwrap_or_default();
    Faults {
        minflt: parse_stat_minflt(&stat).unwrap_or(0),
    }
}

fn parse_stat_minflt(stat: &str) -> Option<u64> {
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(7)?.parse().ok()
}

fn status_value_kb(status: &str, key: &str) -> Option<usize> {
    status.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.trim();
        let kb = value.split_whitespace().next()?.parse().ok()?;
        Some(kb)
    })
}
