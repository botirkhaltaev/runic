#[derive(Clone, Copy)]
pub struct AllocatorTarget {
    name: &'static str,
}

impl AllocatorTarget {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }
}

pub const TARGETS: &[AllocatorTarget] = &[
    AllocatorTarget::new("runic"),
    AllocatorTarget::new("system"),
    AllocatorTarget::new("mimalloc"),
    AllocatorTarget::new("jemalloc"),
    AllocatorTarget::new("snmalloc"),
];

#[must_use]
pub fn by_name(name: &str) -> Option<AllocatorTarget> {
    TARGETS.iter().copied().find(|target| target.name() == name)
}
