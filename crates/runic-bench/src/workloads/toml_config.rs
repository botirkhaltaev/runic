use std::hint::black_box;

const ROUNDS: usize = 16;
const ENTRIES: usize = 128;

pub(super) const ELEMENTS: usize = ROUNDS * ENTRIES;

/// Nested TOML generate → parse → reserialize.
#[must_use]
pub(super) fn run() -> usize {
    toml_config(ROUNDS, ENTRIES)
}

fn toml_config(rounds: usize, entries: usize) -> usize {
    let mut checksum = 0_usize;
    for round in 0..rounds {
        let mut root = toml::map::Map::new();

        let mut app = toml::map::Map::new();
        app.insert("name".into(), toml::Value::String(format!("svc-{round}")));
        app.insert(
            "workers".into(),
            toml::Value::Integer(i64::try_from(entries).unwrap_or(0)),
        );
        app.insert(
            "round".into(),
            toml::Value::Integer(i64::try_from(round).unwrap_or(0)),
        );

        let mut limits = toml::map::Map::new();
        limits.insert("max_conns".into(), toml::Value::Integer(1024));
        limits.insert("idle_ms".into(), toml::Value::Integer(30_000));
        app.insert("limits".into(), toml::Value::Table(limits));
        root.insert("app".into(), toml::Value::Table(app));

        let mut pools = toml::map::Map::new();
        for i in 0..entries {
            let mut pool = toml::map::Map::new();
            pool.insert(
                "size".into(),
                toml::Value::Integer(i64::try_from((i % 32) + 1).unwrap_or(0)),
            );
            pool.insert(
                "timeout_ms".into(),
                toml::Value::Integer(i64::try_from(((i ^ round) % 5_000) + 50).unwrap_or(0)),
            );
            pool.insert("primary".into(), toml::Value::Boolean(i % 3 == 0));
            pools.insert(format!("pool-{i}"), toml::Value::Table(pool));
        }
        root.insert("pools".into(), toml::Value::Table(pools));

        let Ok(text) = toml::to_string(&toml::Value::Table(root)) else {
            continue;
        };
        checksum ^= text.len();
        let Ok(parsed) = toml::from_str::<toml::Value>(&text) else {
            continue;
        };
        let Ok(again) = toml::to_string(&parsed) else {
            continue;
        };
        checksum ^= again.len();
        checksum ^= parsed
            .get("pools")
            .and_then(toml::Value::as_table)
            .map_or(0, toml::map::Map::len);
        black_box((parsed, again));
    }
    black_box(checksum)
}
