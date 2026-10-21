//! B+ tree timings: inserts, point lookups, range scans and deletes straight
//! against `BTree`, plus the same shapes through SQL on an indexed table.
//!
//! Build & run (from the rookdb-cli directory):
//!   cargo build --release --bin btree_bench
//!   ./target/release/btree_bench [rows] [repeats]
//!
//! `rows` defaults to 100,000 and `repeats` to 5; each phase prints the
//! fastest and the median wall time. On a shared machine the fastest run is
//! the steadier figure.

use std::time::Instant;

use storage_manager::backend::engine::Engine;
use storage_manager::backend::index::BTree;
use storage_manager::backend::session::Session;
use storage_manager::catalog::{init_catalog_in, load_catalog_in};
use storage_manager::types::DataType;
use storage_manager::types::value::DataValue;

/// Rows returned by the range scans.
const RANGE_ROWS: usize = 20_000;

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

fn shuffled(n: usize, seed: u64) -> Vec<u32> {
    let mut order: Vec<u32> = (0..n as u32).collect();
    let mut seed = seed;
    for i in (1..order.len()).rev() {
        order.swap(i, (lcg(&mut seed) as usize) % (i + 1));
    }
    order
}

fn report(name: &str, mut times: Vec<f64>, note: &str) {
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "[RESULT] {:<22} min {:>8.2} ms  median {:>8.2} ms  {}",
        name,
        times[0],
        times[times.len() / 2],
        note
    );
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// One tree per repeat: build it, read it, then empty it.
fn bench_tree(
    label: &str,
    key_type: DataType,
    make_key: impl Fn(u32) -> DataValue,
    rows: usize,
    repeats: usize,
) {
    let path = std::env::temp_dir().join(format!(
        "rookdb_btree_bench_{}_{}.idx",
        std::process::id(),
        label
    ));
    let insert_order = shuffled(rows, 0x5EED);
    let lookup_order = shuffled(rows, 0xBEEF);
    let range = RANGE_ROWS.min(rows);
    let low = make_key((rows / 4) as u32);
    let high = make_key((rows / 4 + range - 1) as u32);

    let (mut insert, mut search, mut search_all, mut scan, mut delete) =
        (vec![], vec![], vec![], vec![], vec![]);
    for _ in 0..repeats {
        let mut tree = BTree::create(path.clone(), key_type.clone()).unwrap();

        let t = Instant::now();
        for &n in &insert_order {
            tree.insert(&make_key(n), n / 100, n % 100).unwrap();
        }
        insert.push(ms(t));

        let t = Instant::now();
        for &n in &lookup_order {
            assert!(tree.search(&make_key(n)).unwrap().is_some());
        }
        search.push(ms(t));

        let t = Instant::now();
        for &n in &lookup_order {
            assert_eq!(tree.search_all(&make_key(n)).unwrap().len(), 1);
        }
        search_all.push(ms(t));

        let t = Instant::now();
        for _ in 0..100 {
            assert_eq!(tree.search_range(&low, &high).unwrap().len(), range);
        }
        scan.push(ms(t) / 100.0);

        let t = Instant::now();
        for &n in &lookup_order {
            assert!(tree.delete(&make_key(n), n / 100, n % 100).unwrap());
        }
        delete.push(ms(t));
    }
    let per = format!("({} keys)", rows);
    report(&format!("{}_insert", label), insert, &per);
    report(&format!("{}_search", label), search, &per);
    report(&format!("{}_search_all", label), search_all, &per);
    report(
        &format!("{}_range", label),
        scan,
        &format!("({} entries per scan)", range),
    );
    report(&format!("{}_delete", label), delete, &per);

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}.root", path.display()));
}

fn run(session: &mut Session, sql: &str) -> usize {
    let plan = rook_parser::parse_sql(sql).unwrap_or_else(|e| panic!("{}: {}", sql, e));
    let result = session
        .execute(plan)
        .unwrap_or_else(|e| panic!("{}: {}", sql, e));
    result.rows.map(|(_, rows)| rows.len()).unwrap_or(0)
}

/// The same shapes through the SQL layer, where the index is one cost of many.
fn bench_sql(rows: usize, repeats: usize) {
    let root = std::env::temp_dir().join(format!("rookdb_btree_bench_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let engine = Engine::open(&root);
    init_catalog_in(&engine).expect("init catalog");
    let mut s = Session::new(load_catalog_in(&engine));

    run(&mut s, "CREATE DATABASE bench");
    run(&mut s, "USE bench");
    run(&mut s, "CREATE TABLE t (id INT, k INT, name VARCHAR(20))");
    run(&mut s, "CREATE INDEX t_id ON t (id)");

    let order = shuffled(rows, 0x5EED);
    let load = Instant::now();
    for chunk in order.chunks(500) {
        let values: Vec<String> = chunk
            .iter()
            .map(|i| format!("({}, {}, 'name_{:08}')", i, i % 1000, i))
            .collect();
        run(
            &mut s,
            &format!("INSERT INTO t VALUES {}", values.join(", ")),
        );
    }
    storage_manager::backend::cache::checkpoint_durable().expect("checkpoint");
    println!(
        "[RESULT] {:<22} {:>8.2} ms  ({} rows into an indexed table)",
        "sql_insert",
        ms(load),
        rows
    );

    let lookups = shuffled(rows, 0xBEEF);
    let mut point = vec![];
    for _ in 0..repeats {
        let t = Instant::now();
        for i in lookups.iter().take(2000) {
            assert_eq!(run(&mut s, &format!("SELECT * FROM t WHERE id = {}", i)), 1);
        }
        point.push(ms(t));
    }
    report("sql_point_lookup", point, "(2000 statements)");

    let range = RANGE_ROWS.min(rows);
    let sql = format!(
        "SELECT * FROM t WHERE id BETWEEN {} AND {}",
        rows / 4,
        rows / 4 + range - 1
    );
    let mut scan = vec![];
    for _ in 0..repeats {
        let t = Instant::now();
        assert_eq!(run(&mut s, &sql), range);
        scan.push(ms(t));
    }
    report("sql_range", scan, &format!("({} rows)", range));

    drop(s);
    let _ = std::fs::remove_dir_all(&root);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let repeats: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(5);

    bench_tree(
        "int",
        DataType::Int,
        |n| DataValue::Int(n as i32),
        rows,
        repeats,
    );
    bench_tree(
        "varchar",
        DataType::Varchar(40),
        |n| DataValue::Varchar(format!("k{:08}{}", n, "x".repeat((n % 13) as usize))),
        rows,
        repeats,
    );
    bench_sql(rows, repeats);
}
