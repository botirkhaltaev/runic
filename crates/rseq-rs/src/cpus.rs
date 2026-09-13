//! CPU count from sysfs. Stack buffer only — no `String`.

use std::{fs::File, io::Read};

const PATH: &str = "/sys/devices/system/cpu/possible";

/// Highest listed id plus one (`0-95` → 96).
pub(crate) fn cpus() -> Option<u32> {
    let mut buf = [0u8; 256];
    let n = File::open(PATH).ok()?.read(&mut buf).ok()?;
    let raw = core::str::from_utf8(buf.get(..n)?).ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut max = 0u32;
    for part in raw.split(',') {
        let hi = if let Some((lo, hi)) = part.split_once('-') {
            let _: u32 = lo.parse().ok()?;
            hi.parse().ok()?
        } else {
            part.parse().ok()?
        };
        max = max.max(hi);
    }
    max.checked_add(1)
}

#[cfg(test)]
mod tests {
    use super::cpus;

    #[test]
    fn sysfs() {
        assert!(cpus().is_some_and(|n| n > 0));
    }
}
