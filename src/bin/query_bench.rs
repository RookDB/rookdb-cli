//! Query timings for the scan-heavy statements of the architecture review.
//!
//! Build & run (from the rookdb-cli directory):
//!   cargo build --release --bin query_bench
//!   ./target/release/query_bench [rows] [repeats]
//!
//! Builds `t(id INT, k INT, name VARCHAR(20))` with `rows` rows (default
//! 100,000; `k` has 1,000 distinct values) and the same rows in `p`, which
//! has a primary key on `id` and an index on `k`, in a fresh directory under the
//! system temp dir, then runs each query `repeats` times (default 5) and
//! prints the fastest and the median wall time. On a shared machine the
//! fastest run is the steadier figure.

use std::time::Instant;

use storage_manager::backend::engine::Engine;
use storage_manager::backend::session::Session;
use storage_manager::catalog::{init_catalog_in, load_catalog_in};

const QUERIES: &[(&str, &str)] = &[
    ("count", "SELECT COUNT(*) FROM t"),
    ("sum", "SELECT SUM(k) FROM t"),
    ("group_by", "SELECT k, COUNT(*) FROM t GROUP BY k"),
    (
        "group_by_aggs",
        "SELECT k, COUNT(*), SUM(id), MIN(name) FROM t GROUP BY k",
    ),
    ("group_by_uniq", "SELECT id, COUNT(*) FROM t GROUP BY id"),
    ("order_limit", "SELECT name FROM t ORDER BY name LIMIT 5"),
    ("order_all", "SELECT * FROM t ORDER BY name"),
    ("order_name_only", "SELECT name FROM t ORDER BY name"),
    ("eq_filter", "SELECT * FROM t WHERE id = 77"),
    (
        "range_filter",
        "SELECT * FROM t WHERE id BETWEEN 1000 AND 21000",
    ),
    ("project_int", "SELECT id FROM t"),
    ("project_ints", "SELECT id, k FROM t"),
    ("project_str", "SELECT name FROM t"),
    ("project_all", "SELECT * FROM t"),
    // The same rows in `p`, read through its indexes.
    (
        "pk_range",
        "SELECT * FROM p WHERE id BETWEEN 1000 AND 21000",
    ),
    ("index_eq", "SELECT * FROM p WHERE k = 77"),
];

fn run(session: &mut Session, sql: &str) -> usize {
    let plan = rook_parser::parse_sql(sql).unwrap_or_else(|e| panic!("{}: {}", sql, e));
    let result = session
        .execute(plan)
        .unwrap_or_else(|e| panic!("{}: {}", sql, e));
    result.rows.map(|(_, rows)| rows.len()).unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let repeats: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(5);

    storage_manager::backend::executor::row_select::register_where_parser(
        rook_parser::parse_where_text,
    );
    storage_manager::backend::cache::register_check_parser(rook_parser::parse_check_expr);

    let root = std::env::temp_dir().join(format!("rookdb_query_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::open(&root);
    init_catalog_in(&engine);
    let mut s = Session::new(load_catalog_in(&engine));

    run(&mut s, "CREATE DATABASE bench");
    run(&mut s, "USE bench");
    run(&mut s, "CREATE TABLE t (id INT, k INT, name VARCHAR(20))");
    run(
        &mut s,
        "CREATE TABLE p (id INT PRIMARY KEY, k INT, name VARCHAR(20))",
    );
    run(&mut s, "CREATE INDEX p_k ON p (k)");
    let load = Instant::now();
    for start in (0..rows).step_by(500) {
        let values: Vec<String> = (start..(start + 500).min(rows))
            .map(|i| format!("({}, {}, 'name_{:08}')", i, i % 1000, (i * 7919) % rows))
            .collect();
        let values = values.join(", ");
        run(&mut s, &format!("INSERT INTO t VALUES {}", values));
        run(&mut s, &format!("INSERT INTO p VALUES {}", values));
    }
    storage_manager::backend::cache::checkpoint_durable();
    println!("[LOAD] {} rows in {} ms", rows, load.elapsed().as_millis());

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
            "[RESULT] {:<13} min {:>7.2} ms  median {:>7.2} ms  ({} rows)  {}",
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
