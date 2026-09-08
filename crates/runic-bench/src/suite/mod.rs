use std::time::Duration;

use criterion::Criterion;

pub mod collections;

/// Default Criterion config. The `criterion_group!` macro applies `configure_from_args`
/// so `--measurement-time` / `--sample-size` / `--profile-time` override these defaults.
///
/// Bootstrap is 2000 resamples (not Criterion's 100000): analysis allocates
/// through `#[global_allocator]`, and developer benches do not need publication CIs.
#[must_use]
pub fn criterion() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_secs(1))
        .nresamples(2_000)
        .without_plots()
}

pub(crate) fn elems(n: usize) -> u64 {
    u64::try_from(n).expect("throughput element count exceeds u64")
}
