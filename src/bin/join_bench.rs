//! Hash-join timings for the architecture review (F2/F3).
//!
//! Build & run (from the rookdb-cli directory):
//!   cargo build --release --bin join_bench
//!   ./target/release/join_bench [rows] [repeats]
//!
//! Builds `a(id INT, k INT, name VARCHAR(20))` and `b` with `rows` rows each
//! (default 5,000; `b.id` holds the even numbers, so half of `a` finds a
//! partner) in a fresh directory under the system temp dir, then runs each
//! join `repeats` times (default 7) and prints the fastest and the median
//! wall time. Set `ROOK_HASH_JOIN_BUDGET_BYTES` to time the spill path.

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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(5_000);
    let repeats: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(7);

    storage_manager::backend::executor::row_select::register_where_parser(
        rook_parser::parse_where_text,
    );
    storage_manager::backend::cache::register_check_parser(rook_parser::parse_check_expr);

    let root = std::env::temp_dir().join(format!("rookdb_join_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::open(&root);
    init_catalog_in(&engine);
    let mut s = Session::new(load_catalog_in(&engine));

    run(&mut s, "CREATE DATABASE bench");
    run(&mut s, "USE bench");
    load(&mut s, "a", rows, 1);
    load(&mut s, "b", rows, 2);
    storage_manager::backend::cache::checkpoint_durable();

    for (name, sql) in QUERIES {
        run(&mut s, sql); // warm the caches
        let mut times: Vec<f64> = (0..repeats)
            .map(|_| {
                let t = Instant::now();
                run(&mut s, sql);
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = run(&mut s, sql);
        println!(
            "[RESULT] {:<12} min {:>8.2} ms  median {:>8.2} ms  ({} rows)  {}",
            name,
            times[0],
            times[times.len() / 2],
            n,
            sql
        );
    }

    drop(s);
    let _ = std::fs::remove_dir_all(&root);
}
