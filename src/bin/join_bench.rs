//! Join timings for the architecture review (F2/F3, F4, G17).
//!
//! Build & run (from the rookdb-cli directory):
//!   cargo build --release --bin join_bench
//!   ./target/release/join_bench [rows] [repeats]
//!   ./target/release/join_bench chain [repeats]
//!
//! Builds `a(id INT, k INT, name VARCHAR(20))` and `b` with `rows` rows each
//! (default 5,000; `b.id` holds the even numbers, so half of `a` finds a
//! partner) in a fresh directory under the system temp dir, then runs each
//! join `repeats` times (default 7) and prints the fastest and the median
//! wall time. Set `ROOK_HASH_JOIN_BUDGET_BYTES` to time the spill path.
//!
//! `chain` times join ordering instead: `big` (100,000 rows), `mid`
//! (10,000) and `small` (100), each `big` row naming a `mid` and each `mid`
//! a `small`, joined in a chain written big first, with and without a
//! filter on `small`. Set `ROOK_KEEP_JOIN_ORDER=1` to time the chain in the
//! order written, without join reordering.

use std::time::Instant;

use storage_manager::backend::engine::Engine;
use storage_manager::backend::session::Session;
use storage_manager::catalog::{init_catalog_in, load_catalog_in};

const QUERIES: &[(&str, &str)] = &[
    ("inner", "SELECT a.id, b.name FROM a JOIN b ON a.id = b.id"),
    ("inner_all", "SELECT * FROM a JOIN b ON a.id = b.id"),
    (
        "inner_dup",
        "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.id < 500",
    ),
    (
        "inner_limit",
        "SELECT a.id, b.name FROM a JOIN b ON a.id = b.id LIMIT 10",
    ),
    (
        "left",
        "SELECT a.id, b.name FROM a LEFT JOIN b ON a.id = b.id",
    ),
    ("left_all", "SELECT * FROM a LEFT JOIN b ON a.id = b.id"),
    (
        "left_limit",
        "SELECT a.id, b.name FROM a LEFT JOIN b ON a.id = b.id LIMIT 10",
    ),
    (
        "right",
        "SELECT a.id, b.name FROM a RIGHT JOIN b ON a.id = b.id",
    ),
    (
        "full",
        "SELECT a.id, b.name FROM a FULL JOIN b ON a.id = b.id",
    ),
    // Nested-loop joins (G17): no equality to hash on.
    (
        "nl_lt_limit",
        "SELECT a.id, b.id FROM a JOIN b ON a.k < b.k LIMIT 10",
    ),
    (
        "nl_left_limit",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON a.k < b.k LIMIT 10",
    ),
    ("nl_lt_count", "SELECT COUNT(*) FROM a JOIN b ON a.k < b.k"),
    (
        "nl_full_count",
        "SELECT COUNT(*) FROM a FULL JOIN b ON a.k < b.k AND a.id > b.id",
    ),
    (
        "cross_limit",
        "SELECT a.id, b.id FROM a CROSS JOIN b LIMIT 10",
    ),
    ("cross_count", "SELECT COUNT(*) FROM a CROSS JOIN b"),
    (
        "derived_left",
        "SELECT x.id, x.c, b.name FROM (SELECT id, COUNT(*) c FROM a GROUP BY id) x LEFT JOIN b ON x.id = b.id",
    ),
];

const CHAIN_QUERIES: &[(&str, &str)] = &[
    (
        "chain3",
        "SELECT big.id, mid.w, small.id FROM big JOIN mid ON big.mid_id = mid.id          JOIN small ON mid.small_id = small.id WHERE small.x = 7",
    ),
    (
        "chain3_count",
        "SELECT COUNT(*) FROM big JOIN mid ON big.mid_id = mid.id          JOIN small ON mid.small_id = small.id WHERE small.x = 7",
    ),
    (
        "chain3_midfirst",
        "SELECT big.id, mid.w, small.id FROM mid JOIN big ON big.mid_id = mid.id          JOIN small ON mid.small_id = small.id WHERE small.x = 7",
    ),
    (
        "chain3_nofilt",
        "SELECT COUNT(*) FROM big JOIN mid ON big.mid_id = mid.id          JOIN small ON mid.small_id = small.id",
    ),
    (
        "chain3_agg",
        "SELECT small.x, COUNT(*) FROM big JOIN mid ON big.mid_id = mid.id          JOIN small ON mid.small_id = small.id WHERE small.x < 3 GROUP BY small.x",
    ),
];

fn run(session: &mut Session, sql: &str) -> usize {
    let plan = rook_parser::parse_sql(sql).unwrap_or_else(|e| panic!("{}: {}", sql, e));
    let result = session
        .execute(plan)
        .unwrap_or_else(|e| panic!("{}: {}", sql, e));
    result.rows.map(|(_, rows)| rows.len()).unwrap_or(0)
}

fn load(session: &mut Session, table: &str, rows: usize, id_step: usize) {
    run(
        session,
        &format!("CREATE TABLE {} (id INT, k INT, name VARCHAR(20))", table),
    );
    for start in (0..rows).step_by(500) {
        let values: Vec<String> = (start..(start + 500).min(rows))
            .map(|i| format!("({}, {}, '{}_{:08}')", i * id_step, i % 100, table, i))
            .collect();
        run(
            session,
            &format!("INSERT INTO {} VALUES {}", table, values.join(", ")),
        );
    }
}

/// Create `table` from `ddl` and fill it with `rows` rows, the `i`th
/// given by `row(i)`.
fn load_rows(
    session: &mut Session,
    table: &str,
    ddl: &str,
    rows: usize,
    row: impl Fn(usize) -> String,
) {
    run(session, &format!("CREATE TABLE {} ({})", table, ddl));
    for start in (0..rows).step_by(500) {
        let values: Vec<String> = (start..(start + 500).min(rows)).map(&row).collect();
        run(
            session,
            &format!("INSERT INTO {} VALUES {}", table, values.join(", ")),
        );
    }
}

/// Run each query `repeats` times after a warm-up and print its timings.
fn time_queries(session: &mut Session, queries: &[(&str, &str)], repeats: usize) {
    for (name, sql) in queries {
        run(session, sql); // warm the caches
        let mut times: Vec<f64> = (0..repeats)
            .map(|_| {
                let t = Instant::now();
                run(session, sql);
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = run(session, sql);
        println!(
            "[RESULT] {:<15} min {:>8.2} ms  median {:>8.2} ms  ({} rows)  {}",
            name,
            times[0],
            times[times.len() / 2],
            n,
            sql
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let chain = args.get(1).is_some_and(|a| a == "chain");
    let arg = |i: usize| args.get(i).and_then(|a| a.parse().ok());
    let (rows, repeats): (usize, usize) = if chain {
        (0, arg(2).unwrap_or(7))
    } else {
        (arg(1).unwrap_or(5_000), arg(2).unwrap_or(7))
    };

    let root = std::env::temp_dir().join(format!("rookdb_join_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::open(&root);
    engine.set_keep_join_order(std::env::var_os("ROOK_KEEP_JOIN_ORDER").is_some());
    init_catalog_in(&engine).expect("init catalog");
    let mut s = Session::new(load_catalog_in(&engine));

    run(&mut s, "CREATE DATABASE bench");
    run(&mut s, "USE bench");
    if chain {
        load_rows(&mut s, "big", "id INT, mid_id INT, v INT", 100_000, |i| {
            format!("({}, {}, {})", i, i % 10_000, i % 97)
        });
        load_rows(&mut s, "mid", "id INT, small_id INT, w INT", 10_000, |i| {
            format!("({}, {}, {})", i, i % 100, i % 13)
        });
        load_rows(&mut s, "small", "id INT, x INT", 100, |i| {
            format!("({}, {})", i, i % 50)
        });
        storage_manager::backend::cache::checkpoint_durable();
        time_queries(&mut s, CHAIN_QUERIES, repeats);
    } else {
        load(&mut s, "a", rows, 1);
        load(&mut s, "b", rows, 2);
        storage_manager::backend::cache::checkpoint_durable();
        time_queries(&mut s, QUERIES, repeats);
    }

    drop(s);
    let _ = std::fs::remove_dir_all(&root);
}
