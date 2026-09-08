use crate::{collections, libraries};

#[derive(Clone, Copy)]
pub struct Workload {
    name: &'static str,
    elems: usize,
    run: fn() -> usize,
}

impl Workload {
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    #[must_use]
    pub const fn elems(self) -> usize {
        self.elems
    }

    #[must_use]
    pub fn run(self) -> usize {
        (self.run)()
    }
}

pub const WORKLOADS: &[Workload] = &[
    Workload {
        name: "vec_push_clear",
        elems: 32 * 1_024,
        run: || collections::vec_push_clear(32, 1_024),
    },
    Workload {
        name: "vec_many_small",
        elems: 16 * 1_024,
        run: || collections::vec_many_small(16, 1_024),
    },
    Workload {
        name: "string_building",
        elems: 32 * 1_024,
        run: || collections::string_building(32, 1_024),
    },
    Workload {
        name: "hashmap_insert_remove",
        elems: 16 * 1_024,
        run: || collections::hashmap_insert_remove(16, 1_024),
    },
    Workload {
        name: "arc_clone_drop",
        elems: 32 * 1_024,
        run: || collections::arc_clone_drop(32, 1_024),
    },
    Workload {
        name: "mixed_collections",
        elems: 8 * 1_024,
        run: || collections::mixed_collections(8, 1_024),
    },
    Workload {
        name: "tree",
        elems: 8 * 512,
        run: || collections::tree(8, 512),
    },
    Workload {
        name: "word_count",
        elems: 4 * 4_096,
        run: || collections::word_count(4, 4_096),
    },
    Workload {
        name: "json_api",
        elems: 8 * 128,
        run: || libraries::json_api(8, 128),
    },
    Workload {
        name: "regex_search",
        elems: 8 * 1_024,
        run: || libraries::regex_search(8, 1_024),
    },
    Workload {
        name: "http_buffers",
        elems: 16 * 256,
        run: || libraries::http_buffers(16, 256),
    },
];

#[must_use]
pub fn by_name(name: &str) -> Option<Workload> {
    WORKLOADS
        .iter()
        .copied()
        .find(|workload| workload.name() == name)
}
