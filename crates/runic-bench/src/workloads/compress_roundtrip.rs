use std::hint::black_box;
use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

const ROUNDS: usize = 8;
const KIB: usize = 256;

pub(super) const ELEMENTS: usize = ROUNDS * KIB * 1_024;

/// Gzip compress/decompress of a deterministic text payload (`kib` KiB).
#[must_use]
pub(super) fn run() -> usize {
    compress_roundtrip(ROUNDS, KIB)
}

fn compress_roundtrip(rounds: usize, kib: usize) -> usize {
    let mut checksum = 0_usize;
    let len = kib.saturating_mul(1024);
    for round in 0..rounds {
        let mut payload = String::with_capacity(len);
        let mut line = 0_usize;
        while payload.len() < len {
            payload.push_str("line=");
            payload.push_str(&line.to_string());
            payload.push_str(" round=");
            payload.push_str(&round.to_string());
            payload.push_str(" The quick brown fox jumps over the lazy dog.\n");
            line += 1;
        }
        payload.truncate(len);

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        if encoder.write_all(payload.as_bytes()).is_err() {
            continue;
        }
        let Ok(compressed) = encoder.finish() else {
            continue;
        };
        checksum ^= payload.len() ^ compressed.len();

        let mut decoder = GzDecoder::new(compressed.as_slice());
        let mut output = Vec::new();
        if decoder.read_to_end(&mut output).is_err() {
            continue;
        }
        checksum ^= output.len();
        if let (Some(&a), Some(&b)) = (output.first(), output.last()) {
            checksum ^= usize::from(a) ^ usize::from(b);
        }
        black_box((compressed, output));
    }
    black_box(checksum)
}
