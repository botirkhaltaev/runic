use std::collections::HashMap;
use std::hint::black_box;

const ROUNDS: usize = 4;
const ROWS: usize = 2_048;

pub(super) const ELEMENTS: usize = ROUNDS * ROWS;

/// In-memory CSV write, read-back, and per-category aggregation.
#[must_use]
pub(super) fn run() -> usize {
    csv_pipeline(ROUNDS, ROWS)
}

fn csv_pipeline(rounds: usize, rows: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut writer = csv::Writer::from_writer(Vec::new());
        if writer
            .write_record(["id", "category", "qty", "price"])
            .is_err()
        {
            continue;
        }
        for i in 0..rows {
            let id = (i ^ round).to_string();
            let category = ((i.wrapping_add(round)) % 7).to_string();
            let qty = ((i % 50) + 1).to_string();
            let price = ((i.wrapping_mul(17).wrapping_add(round)) % 10_000).to_string();
            if writer.write_record([&id, &category, &qty, &price]).is_err() {
                break;
            }
        }
        let bytes = match writer.into_inner() {
            Ok(bytes) => bytes,
            Err(error) => error.into_inner().into_inner().unwrap_or_default(),
        };
        checksum ^= bytes.len();

        let mut reader = csv::Reader::from_reader(bytes.as_slice());
        let mut totals: HashMap<String, u64> = HashMap::new();
        let mut count = 0_usize;
        for record in reader.records() {
            let Ok(record) = record else {
                continue;
            };
            let Some(category) = record.get(1) else {
                continue;
            };
            let qty = record
                .get(2)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let price = record
                .get(3)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            *totals.entry(category.to_owned()).or_insert(0) += qty.saturating_mul(price);
            count += 1;
        }
        checksum ^= count ^ totals.len();
        for total in totals.values() {
            checksum ^= usize::try_from(*total).unwrap_or(usize::MAX);
        }
        black_box(totals);
    }
    black_box(checksum)
}
