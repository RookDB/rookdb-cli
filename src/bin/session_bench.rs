//! Per-statement cost of the session layer, with one or more sessions on a
//! data directory.
//!
//! Build & run (from the rookdb-cli directory):
//!   cargo build --release --bin session_bench
//!   ./target/release/session_bench [statements] [repeats]
//!
//! Scenarios (each the fastest of `repeats` runs, default 5):
//!   point_select   one session, `SELECT v FROM p WHERE id = k` (PK lookup)
//!   insert_row     one session, single-row INSERT
//!   own_ddl        one session, CREATE TABLE + INSERT + SELECT on it
//!   other_ddl      session A creates a table, then session B runs a point
//!                  SELECT; B's catalog is out of date every time
//!   threads_N      N threads, one session each, point SELECTs on `p`
//!
//! Tiny statements make any fixed per-statement cost visible.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use storage_manager::backend::engine::Engine;
use storage_manager::backend::session::Session;
use storage_manager::catalog::{init_catalog_in, load_catalog_in};
use storage_manager::types::DataValue;

const P_ROWS: usize = 10_000;

fn run(s: &mut Session, sql: &str) -> Option<DataValue> {
    let plan = rook_parser::parse_sql(sql).unwrap_or_else(|e| panic!("{}: {}", sql, e));
    let result = s.execute(plan).unwrap_or_else(|e| panic!("{}: {}", sql, e));
    result
        .rows
        .and_then(|(_, rows)| rows.into_iter().next())
        .and_then(|t| t.values.into_iter().next().flatten())
}

fn session(root: &Path) -> Session {
    let mut s = Session::new(load_catalog_in(&Engine::open(root)));
    run(&mut s, "USE bench");
    s
}

/// `v` of row `k` of `p`.
fn v_of(k: usize) -> i32 {
    (k * 7 % 1000) as i32
}

fn point_select(s: &mut Session, k: usize) {
    let k = k % P_ROWS;
    let got = run(s, &format!("SELECT v FROM p WHERE id = {}", k));
    assert_eq!(got, Some(DataValue::Int(v_of(k))), "row {}", k);
}

/// Fastest of `repeats` runs of `f(run_number)`, in µs per statement.
fn us_per_stmt(repeats: usize, stmts: usize, mut f: impl FnMut(usize)) -> f64 {
    (0..repeats)
        .map(|r| {
            let t = Instant::now();
            f(r);
            t.elapsed().as_secs_f64() * 1e6 / stmts as f64
        })
        .fold(f64::INFINITY, f64::min)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let stmts: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(10_000);
    let repeats: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(5);

    let root = std::env::temp_dir().join(format!("rookdb_session_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::open(&root);
    init_catalog_in(&engine).expect("init catalog");
    let mut a = Session::new(load_catalog_in(&engine));
    run(&mut a, "CREATE DATABASE bench");
    run(&mut a, "USE bench");
    run(&mut a, "CREATE TABLE p (id INT PRIMARY KEY, v INT)");
    run(&mut a, "CREATE TABLE q (id INT, v INT)");
    for start in (0..P_ROWS).step_by(500) {
        let values: Vec<String> = (start..start + 500)
            .map(|k| format!("({}, {})", k, v_of(k)))
            .collect();
        run(
            &mut a,
            &format!("INSERT INTO p VALUES {}", values.join(", ")),
        );
    }
    storage_manager::backend::cache::checkpoint_durable().expect("checkpoint");

    println!("µs per statement, fastest of {} runs", repeats);

    let t = us_per_stmt(repeats, stmts, |_| {
        for k in 0..stmts {
            point_select(&mut a, k * 13);
        }
    });
    println!("[RESULT] point_select   {:>9.2}", t);

    let mut next = 0usize;
    let t = us_per_stmt(repeats, stmts, |_| {
        for _ in 0..stmts {
            run(&mut a, &format!("INSERT INTO q VALUES ({}, 1)", next));
            next += 1;
        }
    });
    println!("[RESULT] insert_row     {:>9.2}", t);

    // DDL rewrites every system table, so it is slow and its cost grows with
    // the catalog: each iteration drops the table it made, keeping the
    // catalog the same size, and there are few iterations.
    let ddl = 20;
    let t = us_per_stmt(repeats, ddl * 4, |_| {
        for _ in 0..ddl {
            run(&mut a, "CREATE TABLE own_t (id INT)");
            run(&mut a, "INSERT INTO own_t VALUES (1)");
            run(&mut a, "SELECT id FROM own_t");
            run(&mut a, "DROP TABLE own_t");
        }
    });
    println!(
        "[RESULT] own_ddl        {:>9.2}   (CREATE + INSERT + SELECT + DROP, per statement)",
        t
    );

    // Session A changes the schema; B, whose catalog is then out of date,
    // runs one point SELECT. Only B's statement is timed.
    let mut b = session(&root);
    let mut b_us = f64::INFINITY;
    for _ in 0..repeats {
        let mut spent = std::time::Duration::ZERO;
        for k in 0..ddl {
            let ddl_sql = if k % 2 == 0 {
                "CREATE TABLE other_t (id INT)"
            } else {
                "DROP TABLE other_t"
            };
            run(&mut a, ddl_sql);
            let t = Instant::now();
            point_select(&mut b, k * 31);
            spent += t.elapsed();
        }
        b_us = b_us.min(spent.as_secs_f64() * 1e6 / ddl as f64);
    }
    println!(
        "[RESULT] other_ddl      {:>9.2}   (B's point SELECT right after A's DDL)",
        b_us
    );

    for threads in [1usize, 4] {
        let per_thread = stmts / threads;
        let done = AtomicUsize::new(0);
        let t = us_per_stmt(repeats, per_thread * threads, |_| {
            std::thread::scope(|scope| {
                for th in 0..threads {
                    let root = &root;
                    let done = &done;
                    scope.spawn(move || {
                        let mut s = session(root);
                        for k in 0..per_thread {
                            point_select(&mut s, k * 17 + th);
                        }
                        done.fetch_add(per_thread, Ordering::Relaxed);
                    });
                }
            });
        });
        println!(
            "[RESULT] threads_{}      {:>9.2}   ({:.0} statements/s in total)",
            threads,
            t,
            1e6 / t
        );
    }

    drop(a);
    drop(b);
    let _ = std::fs::remove_dir_all(&root);
}
