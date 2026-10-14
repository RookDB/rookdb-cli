//! Memory behaviour with many open tables.
//!
//! Build & run (from the rookdb-cli directory):
//!   cargo build --release --bin pool_bench
//!   /usr/bin/time -v ./target/release/pool_bench [tables] [rows_per_table]
//!
//! Creates `tables` tables of `rows_per_table` rows (defaults 60 and 40,000),
//! then scans every table twice so each one has its pages cached, and prints
//! the scan time. Run it under `/usr/bin/time -v` to read the peak resident
//! set: with one buffer pool per file it grows with the number of tables;
//! with the single global pool it is bounded by the frame budget
//! (`ROOK_POOL_FRAMES`).

use std::time::Instant;

use storage_manager::backend::engine::Engine;
use storage_manager::backend::session::Session;
use storage_manager::catalog::{init_catalog_in, load_catalog_in};

fn run(session: &mut Session, sql: &str) -> usize {
    let plan = rook_parser::parse_sql(sql).unwrap_or_else(|e| panic!("{}: {}", sql, e));
    let result = session
        .execute(plan)
        .unwrap_or_else(|e| panic!("{}: {}", sql, e));
    result.rows.map(|(_, rows)| rows.len()).unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tables: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(60);
    let rows: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(40_000);

    let root = std::env::temp_dir().join(format!("rookdb_pool_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::open(&root);
    init_catalog_in(&engine).expect("init catalog");
    let mut s = Session::new(load_catalog_in(&engine));

    run(&mut s, "CREATE DATABASE bench");
    run(&mut s, "USE bench");
    let load = Instant::now();
    for t in 0..tables {
        run(
            &mut s,
            &format!("CREATE TABLE t{t} (id INT PRIMARY KEY, k INT, name VARCHAR(20))"),
        );
        for start in (0..rows).step_by(500) {
            let values: Vec<String> = (start..(start + 500).min(rows))
                .map(|i| format!("({}, {}, 'name_{:08}')", i, i % 1000, i))
                .collect();
            run(
                &mut s,
                &format!("INSERT INTO t{t} VALUES {}", values.join(", ")),
            );
        }
    }
    storage_manager::backend::cache::checkpoint_durable();
    println!(
        "[LOAD] {} tables x {} rows in {} ms",
        tables,
        rows,
        load.elapsed().as_millis()
    );

    for pass in 0..2 {
        let t = Instant::now();
        let mut n = 0;
        for i in 0..tables {
            n += run(&mut s, &format!("SELECT * FROM t{i}"));
        }
        println!(
            "[SCAN] pass {} read {} rows in {} ms",
            pass,
            n,
            t.elapsed().as_millis()
        );
    }

    drop(s);
    let _ = std::fs::remove_dir_all(&root);
}
