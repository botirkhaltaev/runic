mod arc_broadcast;
mod async_server;
mod buffer_pool;
mod compress_roundtrip;
mod csv_pipeline;
mod graph_shortest_path;
mod hashmap_grow;
mod http_parse;
mod json_api;
mod log_pipeline;
mod lru_cache;
mod records_sort;
mod regex_search;
mod shard_aggregator;
mod text_index;
mod thread_pool_jobs;
mod toml_config;
mod vec_growth_log;
mod vecdeque_events;
mod word_count;

#[derive(Clone, Copy)]
pub struct Workload {
    name: &'static str,
    elements: usize,
    run: fn() -> usize,
}

impl Workload {
    const fn new(name: &'static str, elements: usize, run: fn() -> usize) -> Self {
        Self {
            name,
            elements,
            run,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    #[must_use]
    pub const fn elements(self) -> usize {
        self.elements
    }

    #[must_use]
    pub fn run(self) -> usize {
        (self.run)()
    }
}

macro_rules! workload {
    ($module:ident) => {
        Workload::new(stringify!($module), $module::ELEMENTS, $module::run)
    };
}

pub const WORKLOADS: &[Workload] = &[
    workload!(word_count),
    workload!(vec_growth_log),
    workload!(hashmap_grow),
    workload!(vecdeque_events),
    workload!(text_index),
    workload!(lru_cache),
    workload!(records_sort),
    workload!(graph_shortest_path),
    workload!(json_api),
    workload!(regex_search),
    workload!(http_parse),
    workload!(csv_pipeline),
    workload!(compress_roundtrip),
    workload!(toml_config),
    workload!(async_server),
    workload!(thread_pool_jobs),
    workload!(log_pipeline),
    workload!(shard_aggregator),
    workload!(buffer_pool),
    workload!(arc_broadcast),
];
