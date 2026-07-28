use std::time::Duration;

use criterion::{BenchmarkGroup, measurement::WallTime};

/// Default Criterion tuning for developer-sized iteration runs.
pub fn configure_group(group: &mut BenchmarkGroup<'_, WallTime>) {
    group
        .sample_size(10)
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_secs(1));
}
