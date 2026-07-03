use std::{
    fs,
    time::{Duration, Instant},
};

pub struct RssSample {
    pub current_bytes: usize,
    pub peak_bytes: usize,
}

pub struct RssReport {
    pub allocator: &'static str,
    pub workload: &'static str,
    pub ops: usize,
    pub elapsed: Duration,
    pub before: RssSample,
    pub after: RssSample,
}

impl RssReport {
    #[must_use]
    pub fn measure(
        allocator: &'static str,
        workload: &'static str,
        ops: usize,
        run: impl FnOnce(),
    ) -> Self {
        let before = RssSample::read();
        let started = Instant::now();
        run();
        let elapsed = started.elapsed();
        let after = RssSample::read();

        Self {
            allocator,
            workload,
            ops,
            elapsed,
            before,
            after,
        }
    }

    pub fn print_csv_header() {
        println!(
            "allocator,workload,ops,elapsed_ns,rss_before_bytes,rss_after_bytes,rss_delta_bytes,peak_rss_bytes"
        );
    }

    pub fn print_csv(&self) {
        println!(
            "{},{},{},{},{},{},{},{}",
            self.allocator,
            self.workload,
            self.ops,
            self.elapsed.as_nanos(),
            self.before.current_bytes,
            self.after.current_bytes,
            self.after.current_bytes.cast_signed() - self.before.current_bytes.cast_signed(),
            self.after.peak_bytes.max(self.before.peak_bytes),
        );
    }
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

fn status_value_kb(status: &str, key: &str) -> Option<usize> {
    status.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.trim();
        let kb = value.split_whitespace().next()?.parse().ok()?;
        Some(kb)
    })
}
