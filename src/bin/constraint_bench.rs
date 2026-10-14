//! Timings for the statements whose constraint checks used to scan a table.
//!
//! Build & run (from the rookdb-cli directory):
//!   cargo build --release --bin constraint_bench
//!   ./target/release/constraint_bench [repeats]
//!
//! Each repeat builds fresh tables in a new directory under the system temp
//! dir and times three statements:
//!   insert   one 10,000-row INSERT into a child table with a foreign key to
//!            a 10,000-row parent and a UNIQUE column
//!   cascade  DELETE of 1,000 parents whose 10 children each are cascaded
//!   restrict the same DELETE blocked by a RESTRICT key, the last parent
//!            checked being the one with children
//! and prints the fastest and the median wall time of the repeats.

use std::time::Instant;

use storage_manager::backend::engine::Engine;
use storage_manager::backend::session::Session;
use storage_manager::catalog::{init_catalog_in, load_catalog_in};

const PARENTS: usize = 10_000;
const DELETED: usize = 1_000;
const CHILDREN_PER_PARENT: usize = 10;

fn run(session: &mut Session, sql: &str) -> Result<usize, String> {
    let plan = rook_parser::parse_sql(sql).map_err(|e| format!("{}: {}", sql, e))?;
    let result = session
        .execute(plan)
        .map_err(|e| format!("{}: {}", sql, e))?;
    Ok(result.rows.map(|(_, rows)| rows.len()).unwrap_or(0))
}

fn run_ok(session: &mut Session, sql: &str) {
    run(session, sql).unwrap_or_else(|e| panic!("{}", e));
}

fn tuples(range: std::ops::Range<usize>, row: impl Fn(usize) -> String) -> String {
    range.map(row).collect::<Vec<_>>().join(", ")
}

fn fresh_session(tag: &str) -> (Session, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "rookdb_constraint_bench_{}_{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::open(&root);
    init_catalog_in(&engine);
    let mut s = Session::new(load_catalog_in(&engine));
    run_ok(&mut s, "CREATE DATABASE bench");
    run_ok(&mut s, "USE bench");
    (s, root)
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// A 10,000-row INSERT into a child with a foreign key and a UNIQUE column.
fn bench_insert() -> f64 {
    let (mut s, root) = fresh_session("insert");
    run_ok(&mut s, "CREATE TABLE p (id INT PRIMARY KEY)");
    run_ok(
        &mut s,
        "CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id), u INT UNIQUE)",
    );
    run_ok(
        &mut s,
        &format!(
            "INSERT INTO p VALUES {}",
            tuples(0..PARENTS, |i| format!("({})", i))
        ),
    );
    let sql = format!(
        "INSERT INTO c VALUES {}",
        tuples(0..PARENTS, |i| format!("({}, {}, {})", i, i, i * 3))
    );
    let t = Instant::now();
    run_ok(&mut s, &sql);
    let elapsed = ms(t);
    storage_manager::backend::cache::checkpoint_durable();
    drop(s);
    let _ = std::fs::remove_dir_all(&root);
    elapsed
}

/// Parents `0..PARENTS`; every parent below `DELETED` has children (CASCADE),
/// or, when `restrict_last`, only parent `DELETED - 1` has them and the key
/// is RESTRICT.
fn bench_delete(action: &str, tag: &str) -> f64 {
    let (mut s, root) = fresh_session(tag);
    run_ok(&mut s, "CREATE TABLE p (id INT PRIMARY KEY)");
    run_ok(
        &mut s,
        &format!(
            "CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id) {})",
            action
        ),
    );
    run_ok(
        &mut s,
        &format!(
            "INSERT INTO p VALUES {}",
            tuples(0..PARENTS, |i| format!("({})", i))
        ),
    );
    // Cascade: children under every deleted parent. Restrict: only under the
    // last one, so the check scans for every parent before it finds a hit.
    let parents = if action.is_empty() {
        DELETED - 1..DELETED
    } else {
        0..DELETED
    };
    let first = parents.start;
    let children = parents.len() * CHILDREN_PER_PARENT;
    run_ok(
        &mut s,
        &format!(
            "INSERT INTO c VALUES {}",
            tuples(0..children, |i| format!(
                "({}, {})",
                i,
                first + i / CHILDREN_PER_PARENT
            ))
        ),
    );
    let t = Instant::now();
    let result = run(&mut s, &format!("DELETE FROM p WHERE id < {}", DELETED));
    let elapsed = ms(t);
    if action.is_empty() {
        assert!(result.is_err(), "RESTRICT delete should be blocked");
    } else {
        result.unwrap_or_else(|e| panic!("{}", e));
    }
    drop(s);
    let _ = std::fs::remove_dir_all(&root);
    elapsed
}

fn report(name: &str, mut times: Vec<f64>) {
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "[RESULT] {:<9} min {:>9.1} ms  median {:>9.1} ms",
        name,
        times[0],
        times[times.len() / 2]
    );
}

fn main() {
    let repeats: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(3);

    report("insert", (0..repeats).map(|_| bench_insert()).collect());
    report(
        "cascade",
        (0..repeats)
            .map(|_| bench_delete("ON DELETE CASCADE", "cascade"))
            .collect(),
    );
    report(
        "restrict",
        (0..repeats).map(|_| bench_delete("", "restrict")).collect(),
    );
}
