use runic::RunicAlloc;
use std::{collections::HashMap, sync::Arc};

#[global_allocator]
static GLOBAL: RunicAlloc = RunicAlloc::new();

#[test]
fn box_uses_runic() {
    let value = Box::new(42_u64);
    assert_eq!(*value, 42);
}

#[test]
fn vec_uses_runic() {
    let mut values = Vec::new();

    for value in 0..10_000 {
        values.push(value);
    }

    assert_eq!(values.len(), 10_000);
    assert_eq!(values[4096], 4096);
}

#[test]
fn string_uses_runic() {
    let mut value = String::new();

    for _ in 0..1024 {
        value.push_str("runic");
    }

    assert!(value.starts_with("runic"));
    assert_eq!(value.len(), 1024 * "runic".len());
}

#[test]
fn hash_map_uses_runic() {
    let mut values = HashMap::new();

    for value in 0..2048 {
        values.insert(value, value * 2);
    }

    assert_eq!(values.get(&1024), Some(&2048));
}

#[test]
fn arc_uses_runic() {
    let value = Arc::new(String::from("runic"));
    let cloned = Arc::clone(&value);

    assert_eq!(&**cloned, "runic");
    assert_eq!(Arc::strong_count(&value), 2);
}

#[test]
fn nested_vec_string_uses_runic() {
    let mut values = Vec::new();

    for index in 0..512 {
        values.push(format!("runic-{index}"));
    }

    assert_eq!(values[511], "runic-511");
}
