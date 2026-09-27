//! `RunicAlloc` as the process `#[global_allocator]`: standard collections
//! keep their contents through growth, hand-off between threads, and drops on
//! threads other than the one that allocated.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

use runic::RunicAlloc;

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::new();

fn checksum(values: &[u64]) -> u64 {
    values
        .iter()
        .fold(0_u64, |acc, &value| acc.rotate_left(5) ^ value)
}

/// Vectors grow through every realloc step and keep their contents; strings
/// and maps built alongside them stay intact.
#[test]
fn collections_keep_their_contents_through_growth() {
    let mut vectors: Vec<Vec<u64>> = Vec::new();
    let mut strings: Vec<String> = Vec::new();
    let mut map: HashMap<usize, Vec<u8>> = HashMap::new();

    for index in 0..512_usize {
        let len = index * 37 % 5000;
        vectors.push(pattern(index, len));
        strings.push(
            (0..len % 200)
                .map(|n| char::from(b'a' + u8::try_from(n % 26).unwrap()))
                .collect(),
        );
        map.insert(index, vec![u8::try_from(index % 256).unwrap(); len % 300]);
    }

    for (index, values) in vectors.iter().enumerate() {
        let expected = pattern(index, values.len());
        assert_eq!(checksum(values), checksum(&expected), "vector {index}");
        assert!(strings[index].bytes().all(|byte| byte.is_ascii_lowercase()));
        let byte = u8::try_from(index % 256).unwrap();
        assert!(map[&index].iter().all(|&found| found == byte));
    }
}

fn pattern(index: usize, len: usize) -> Vec<u64> {
    let factor = u64::try_from(index + 1).unwrap();
    (0..u64::try_from(len).unwrap())
        .map(|n| n.wrapping_mul(factor))
        .collect()
}

/// `vec![0; n]` goes through `alloc_zeroed`; after the same sizes were filled
/// and dropped, every element is still zero.
#[test]
fn zeroed_vectors_are_zero_after_dirty_reuse() {
    for len in [8, 100, 4096, 40_000, 200_000] {
        let dirty = vec![0xff_u8; len];
        assert!(dirty.iter().all(|&byte| byte == 0xff));
        drop(dirty);

        let clean = vec![0_u8; len];
        assert!(clean.iter().all(|&byte| byte == 0), "len {len}");
    }
}

/// Collections built on worker threads are read and dropped on the main
/// thread after the workers have exited, and collections built on the main
/// thread are dropped on workers.
#[test]
fn collections_move_between_threads_and_outlive_their_builders() {
    const WORKERS: usize = 8;

    let built: Vec<(u64, Vec<u64>)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..u64::try_from(WORKERS).unwrap())
            .map(|worker| {
                scope.spawn(move || {
                    let values: Vec<u64> = (0..20_000).map(|n| n ^ worker).collect();
                    (checksum(&values), values)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    for (expected, values) in built {
        assert_eq!(checksum(&values), expected);
    }

    let (tx, rx) = mpsc::channel::<(usize, Vec<String>)>();
    thread::scope(|scope| {
        scope.spawn(move || {
            for (expected, strings) in rx {
                let found = strings.iter().map(String::len).sum::<usize>();
                assert_eq!(found, expected);
            }
        });
        for round in 0..64 {
            let strings: Vec<String> = (0..round * 4).map(|n| "x".repeat(n)).collect();
            let expected = strings.iter().map(String::len).sum::<usize>();
            tx.send((expected, strings)).unwrap();
        }
        drop(tx);
    });
}

/// Many short-lived threads each allocate and free; the process keeps working
/// after their heaps have drained.
#[test]
fn thread_churn_leaves_the_allocator_usable() {
    for round in 0..16 {
        let handles: Vec<_> = (0..8_u8)
            .map(|worker| {
                thread::spawn(move || {
                    let mut total = 0_usize;
                    for n in 0..200 {
                        let block = vec![worker; (n * 13 + round) % 3000 + 1];
                        total += block.len();
                    }
                    total
                })
            })
            .collect();
        for handle in handles {
            assert!(handle.join().unwrap() > 0);
        }
    }

    let survivor: Vec<u64> = (0..100_000).collect();
    assert_eq!(survivor.iter().sum::<u64>(), 4_999_950_000);
}
