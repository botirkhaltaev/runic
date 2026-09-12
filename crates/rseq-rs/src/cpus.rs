//! Possible CPU count from sysfs. No alloc.

const PATH: &[u8] = b"/sys/devices/system/cpu/possible\0";

pub(crate) fn possible() -> Option<u32> {
    let mut buf = [0u8; 256];
    let n = read_path(PATH, &mut buf)?;
    parse(&buf[..n])
}

fn read_path(path: &[u8], buf: &mut [u8]) -> Option<usize> {
    if path.last() != Some(&0) {
        return None;
    }
    // SAFETY: `path` is a NUL-terminated C string. `buf` is writable.
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is open; `buf` is valid for writes.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    // SAFETY: `fd` is the descriptor we opened.
    unsafe { libc::close(fd) };
    if n <= 0 {
        return None;
    }
    usize::try_from(n).ok()
}

/// Highest listed id plus one (`0-95` → 96).
fn parse(raw: &[u8]) -> Option<u32> {
    let s = trim(raw);
    if s.is_empty() {
        return None;
    }
    let mut max = 0u32;
    let mut i = 0;
    while i < s.len() {
        let start = i;
        while i < s.len() && s[i] != b',' {
            i += 1;
        }
        max = max.max(parse_part(&s[start..i])?);
        if i < s.len() {
            i += 1;
        }
    }
    max.checked_add(1)
}

fn parse_part(part: &[u8]) -> Option<u32> {
    if let Some(dash) = part.iter().position(|c| *c == b'-') {
        let hi = parse_u32(&part[dash.saturating_add(1)..])?;
        let _lo = parse_u32(&part[..dash])?;
        Some(hi)
    } else {
        parse_u32(part)
    }
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut n = 0u32;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u32::from(c - b'0'))?;
    }
    Some(n)
}

fn trim(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && s[a].is_ascii_whitespace() {
        a += 1;
    }
    while b > a && s[b - 1].is_ascii_whitespace() {
        b -= 1;
    }
    s.get(a..b).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn range() {
        assert_eq!(parse(b"0-95\n"), Some(96));
    }

    #[test]
    fn list() {
        assert_eq!(parse(b"0,3-5"), Some(6));
    }

    #[test]
    fn single() {
        assert_eq!(parse(b"0\n"), Some(1));
    }
}
