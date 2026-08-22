//! Index-acceleration end-to-end tests.
//!
//! With an index present, the physical planner rewrites equality, BETWEEN,
//! single-element IN and inequality predicates into PointLookup / RangeLookup
//! scans instead of SeqScan+Filter.
//!
//! Run with: cargo build -p rookdb-cli
//!           cargo test --test index_acceleration -- --test-threads=1

use std::io::Write;
use std::process::{Command, Stdio};

fn workspace(name: &str) -> String {
    let dir = format!(
        "{}/database_idxaccel_{}_{}",
        env!("CARGO_MANIFEST_DIR"),
        std::process::id(),
        name
    );
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rookdb_bin() -> String {
    env!("CARGO_BIN_EXE_rookdb").to_string()
}

fn rook(ws: &str, sql_lines: &[&str]) -> String {
    let mut child = Command::new(rookdb_bin())
        .current_dir(ws)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn rookdb");
    {
        let stdin = child.stdin.as_mut().unwrap();
        for line in sql_lines {
            writeln!(stdin, "{}", line).unwrap();
        }
        writeln!(stdin, "exit").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn setup(name: &str) -> String {
    let ws = workspace(name);
    let out = rook(
        &ws,
        &[
            "CREATE DATABASE pay;",
            "USE pay;",
            "CREATE TABLE emp (id INT, name VARCHAR(30), salary INT);",
            "INSERT INTO emp VALUES (1,'Ann',50000);",
            "INSERT INTO emp VALUES (2,'Ben',62000);",
            "INSERT INTO emp VALUES (3,'Cy',75000);",
            "INSERT INTO emp VALUES (4,'Dee',91000);",
            "CREATE INDEX by_salary ON emp(salary);",
        ],
    );
    assert!(out.contains("Created index"), "setup failed:\n{}", out);
    ws
}

#[test]
fn between_uses_the_index() {
    let ws = setup("between");
    let out = rook(
        &ws,
        &["USE pay;", "SELECT name FROM emp WHERE salary BETWEEN 50000 AND 80000;"],
    );
    // Bounds are inclusive: Ann (50k), Ben (62k), Cy (75k) match.
    assert!(out.contains("3 row(s) returned"), "output:\n{}", out);
    assert!(out.contains("'Ann'") && out.contains("'Ben'") && out.contains("'Cy'"),
            "output:\n{}", out);
    assert!(!out.contains("'Dee'"), "Dee must be excluded:\n{}", out);
}

#[test]
fn greater_than_is_range_accelerated() {
    let ws = setup("gt");
    let out = rook(&ws, &["USE pay;", "SELECT name FROM emp WHERE salary > 70000;"]);
    assert!(out.contains("2 row(s) returned"), "output:\n{}", out);
    assert!(out.contains("'Cy'") && out.contains("'Dee'"), "output:\n{}", out);
}

#[test]
fn less_than_equal_is_range_accelerated() {
    let ws = setup("le");
    let out = rook(&ws, &["USE pay;", "SELECT name FROM emp WHERE salary <= 62000;"]);
    assert!(out.contains("2 row(s) returned"), "output:\n{}", out);
    assert!(out.contains("'Ann'") && out.contains("'Ben'"), "output:\n{}", out);
}

#[test]
fn and_range_combines_into_one_interval() {
    let ws = setup("andrange");
    let out = rook(
        &ws,
        &["USE pay;", "SELECT name FROM emp WHERE salary > 50000 AND salary < 91000;"],
    );
    // Ben (62k) and Cy (75k); bounds are exclusive.
    assert!(out.contains("2 row(s) returned"), "output:\n{}", out);
}

#[test]
fn point_lookup_on_indexed_equality() {
    let ws = setup("point");
    let out = rook(&ws, &["USE pay;", "SELECT name FROM emp WHERE salary = 91000;"]);
    assert!(out.contains("1 row(s) returned"), "output:\n{}", out);
    assert!(out.contains("'Dee'"), "output:\n{}", out);
}
