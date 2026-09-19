use std::hint::black_box;

use regex::Regex;

const ROUNDS: usize = 8;
const LINES: usize = 1_024;

pub(super) const ELEMENTS: usize = ROUNDS * LINES;

/// Log-scan traffic: compile a pattern and search generated lines.
#[must_use]
pub(super) fn run() -> usize {
    regex_search(ROUNDS, LINES)
}

fn regex_search(rounds: usize, lines: usize) -> usize {
    let Ok(re) = Regex::new(r"(?i)\b(error|warn|user_\d+)\b") else {
        return 0;
    };
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut haystack = String::with_capacity(lines * 40);
        for i in 0..lines {
            haystack.push_str(match (i ^ round) % 5 {
                0 => "ERROR user_",
                1 => "WARN cache miss user_",
                2 => "INFO accepted user_",
                _ => "DEBUG skip user_",
            });
            haystack.push_str(&(i % 97).to_string());
            haystack.push('\n');
        }
        checksum ^= re.find_iter(&haystack).count();
        black_box(haystack);
    }
    black_box(checksum)
}
