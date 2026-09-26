use core::ffi::CStr;

use crate::heap::extent::config::{ExtentConfig, ExtentPolicy};
use crate::heap::run::config::{RunConfig, RunPolicy};

/// Payload hugepage policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HugePage {
    /// Ordinary anonymous pages, with no hugepage advice.
    #[default]
    Off,
    /// Ordinary mapping followed by the best-effort `MADV_HUGEPAGE` hint.
    Thp,
}

/// Payload NUMA policy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Numa {
    /// Kernel first-touch placement.
    #[default]
    Off,
    /// Best-effort `MPOL_PREFERRED` placement on the allocating thread's node.
    Local,
}

/// Best-effort hugepage and NUMA hints for a payload map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Hints {
    hugepage: HugePage,
    numa: Numa,
}

impl Hints {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            hugepage: HugePage::Off,
            numa: Numa::Off,
        }
    }

    #[must_use]
    pub(crate) const fn hugepage(self) -> HugePage {
        self.hugepage
    }

    #[must_use]
    pub(crate) const fn numa(self) -> Numa {
        self.numa
    }

    #[must_use]
    pub(crate) const fn with_hugepage(mut self, hugepage: HugePage) -> Self {
        self.hugepage = hugepage;
        self
    }

    #[must_use]
    pub(crate) const fn with_numa(mut self, numa: Numa) -> Self {
        self.numa = numa;
        self
    }
}

impl Default for Hints {
    fn default() -> Self {
        Self::new()
    }
}

/// Immutable allocator configuration for tunable allocator behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AllocatorConfig {
    extent: ExtentConfig,
    run: RunConfig,
    hints: Hints,
}

impl AllocatorConfig {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            extent: ExtentConfig::new(),
            run: RunConfig::new(),
            hints: Hints::new(),
        }
    }

    #[must_use]
    pub const fn extent(self) -> ExtentConfig {
        self.extent
    }

    #[must_use]
    pub const fn run(self) -> RunConfig {
        self.run
    }

    #[must_use]
    pub(crate) const fn hints(self) -> Hints {
        self.hints
    }

    #[must_use]
    pub const fn with_extent_config(mut self, extent: ExtentConfig) -> Self {
        self.extent = extent;
        self
    }

    #[must_use]
    pub const fn with_run_config(mut self, run: RunConfig) -> Self {
        self.run = run;
        self
    }

    #[must_use]
    pub const fn with_hugepage(mut self, hugepage: HugePage) -> Self {
        self.hints = self.hints.with_hugepage(hugepage);
        self
    }

    #[must_use]
    pub const fn with_numa(mut self, numa: Numa) -> Self {
        self.hints = self.hints.with_numa(numa);
        self
    }

    /// Read preload configuration from `RUNIC_*`.
    ///
    /// Starts from [`Self::new`]. Unknown enum values, malformed integers, and
    /// integer overflow leave that setting at its default.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new().overlay_env()
    }

    #[must_use]
    pub(crate) fn overlay_env(self) -> Self {
        let mut config = self;
        config = overlay_var(config, c"RUNIC_HUGEPAGE");
        config = overlay_var(config, c"RUNIC_NUMA");
        config = overlay_var(config, c"RUNIC_EXTENT_POLICY");
        config = overlay_var(config, c"RUNIC_EXTENT_SLOTS");
        config = overlay_var(config, c"RUNIC_EXTENT_BYTES");
        config = overlay_var(config, c"RUNIC_RUN_POLICY");
        config
    }

    #[must_use]
    pub(crate) fn overlay(self, name: &[u8], value: &[u8]) -> Self {
        match name {
            b"RUNIC_HUGEPAGE" => match value {
                b"off" => self.with_hugepage(HugePage::Off),
                b"thp" => self.with_hugepage(HugePage::Thp),
                _ => self,
            },
            b"RUNIC_NUMA" => match value {
                b"off" => self.with_numa(Numa::Off),
                b"local" => self.with_numa(Numa::Local),
                _ => self,
            },
            b"RUNIC_EXTENT_POLICY" => match value {
                b"keep" => self.with_extent_config(self.extent.with_policy(ExtentPolicy::Keep)),
                b"discard" => {
                    self.with_extent_config(self.extent.with_policy(ExtentPolicy::Discard))
                }
                b"unmap" => self.with_extent_config(self.extent.with_policy(ExtentPolicy::Unmap)),
                _ => self,
            },
            b"RUNIC_RUN_POLICY" => match value {
                b"keep" => self.with_run_config(self.run.with_policy(RunPolicy::Keep)),
                b"discard" => self.with_run_config(self.run.with_policy(RunPolicy::Discard)),
                _ => self,
            },
            b"RUNIC_EXTENT_SLOTS" => parse_usize(value).map_or(self, |slots| {
                self.with_extent_config(
                    self.extent
                        .with_budget(Budget::new(slots, self.extent.budget().bytes())),
                )
            }),
            b"RUNIC_EXTENT_BYTES" => parse_usize(value).map_or(self, |bytes| {
                self.with_extent_config(
                    self.extent
                        .with_budget(Budget::new(self.extent.budget().slots(), bytes)),
                )
            }),
            _ => self,
        }
    }
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Slot and byte limits for allocator mapping caches.
///
/// Slot and byte limits are enforced exactly. There is no internal clamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    slots: usize,
    bytes: usize,
}

impl Budget {
    #[must_use]
    pub const fn new(slots: usize, bytes: usize) -> Self {
        Self { slots, bytes }
    }

    #[must_use]
    pub const fn slots(self) -> usize {
        self.slots
    }

    #[must_use]
    pub const fn bytes(self) -> usize {
        self.bytes
    }
}

fn overlay_var(config: AllocatorConfig, name: &CStr) -> AllocatorConfig {
    // SAFETY: `name` is a live C string. The returned pointer is consumed
    // before returning and is never retained across an environment mutation.
    let value = unsafe { libc::getenv(name.as_ptr()) };
    if value.is_null() {
        return config;
    }
    // SAFETY: a non-null getenv result is a NUL-terminated value. It is
    // borrowed only for this call.
    let value = unsafe { CStr::from_ptr(value) };
    config.overlay(name.to_bytes(), value.to_bytes())
}

fn parse_usize(value: &[u8]) -> Option<usize> {
    core::str::from_utf8(value).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_ignores_unknown_values() {
        let base = AllocatorConfig::new();
        assert_eq!(base.overlay(b"RUNIC_HUGEPAGE", b"huge"), base);
        assert_eq!(base.overlay(b"RUNIC_HUGEPAGE", b"force"), base);
        assert_eq!(base.overlay(b"RUNIC_NUMA", b"bind"), base);
        assert_eq!(base.overlay(b"RUNIC_EXTENT_POLICY", b"cache"), base);
        assert_eq!(base.overlay(b"RUNIC_RUN_POLICY", b"unmap"), base);
        assert_eq!(base.overlay(b"RUNIC_EXTENT_SLOTS", b""), base);
        assert_eq!(base.overlay(b"RUNIC_EXTENT_SLOTS", b"-1"), base);
        assert_eq!(base.overlay(b"RUNIC_EXTENT_BYTES", b"1M"), base);
        assert_eq!(
            base.overlay(b"RUNIC_EXTENT_BYTES", b"999999999999999999999999999999"),
            base
        );
        assert_eq!(base.overlay(b"RUNIC_UNKNOWN", b"fast"), base);
    }

    #[test]
    fn overlay_sets_known_keys() {
        let config = AllocatorConfig::new()
            .overlay(b"RUNIC_HUGEPAGE", b"thp")
            .overlay(b"RUNIC_NUMA", b"local")
            .overlay(b"RUNIC_EXTENT_POLICY", b"unmap")
            .overlay(b"RUNIC_RUN_POLICY", b"discard")
            .overlay(b"RUNIC_EXTENT_SLOTS", b"4")
            .overlay(b"RUNIC_EXTENT_BYTES", b"1024");
        assert_eq!(config.hints().hugepage(), HugePage::Thp);
        assert_eq!(config.hints().numa(), Numa::Local);
        assert_eq!(config.extent().policy(), ExtentPolicy::Unmap);
        assert_eq!(config.run().policy(), RunPolicy::Discard);
        assert_eq!(config.extent().budget().slots(), 4);
        assert_eq!(config.extent().budget().bytes(), 1024);
    }
}
