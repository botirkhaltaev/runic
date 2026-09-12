//! Possible CPU count from sysfs. Stack buffer only — no `String`.

use std::{fs::File, io::Read};

const PATH: &str = "/sys/devices/system/cpu/possible";

pub(crate) fn possible() -> Option<u32> {
    let mut buf = [0u8; 256];
    let n = File::open(PATH).ok()?.read(&mut buf).ok()?;
    parse(core::str::from_utf8(buf.get(..n)?).ok()?)
}

/// Highest listed id plus one (`0-95` → 96).
fn parse(raw: &str) -> Option<u32> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let mut max = 0u32;
    for part in s.split(',') {
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
    use super::parse;

    #[test]
    fn range() {
        assert_eq!(parse("0-95\n"), Some(96));
    }

    #[test]
    fn list() {
        assert_eq!(parse("0,3-5"), Some(6));
    }

    #[test]
    fn single() {
        assert_eq!(parse("0\n"), Some(1));
    }
}
