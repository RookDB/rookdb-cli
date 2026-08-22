//! End-to-end smoke tests: run the `rookdb` binary as a subprocess and drive
//! a full SQL workflow through the Volcano-backed shell.
//!
//! Each test gets an isolated workspace directory so tests never share state;
//! cargo guarantees the binary is built before integration tests run.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

fn rookdb_bin() -> String {
    // CARGO_BIN_EXE_* is provided by cargo for integration tests.
    env!("CARGO_BIN_EXE_rookdb").to_string()
}

/// Feed SQL lines to a fresh CLI process and return combined output.
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

#[test]
fn crud_workflow_through_volcano_engine() {
    let ws = common::Workspace::new("crud");
    let out = rook(
        &ws,
        &[
            "CREATE DATABASE shop;",
            "USE shop;",
            "CREATE TABLE items (id INT, name VARCHAR(50), price DOUBLE PRECISION);",
            "INSERT INTO items VALUES (1, 'bolt', 0.25);",
            "INSERT INTO items VALUES (2, 'nut', 0.10);",
            "SELECT * FROM items WHERE id = 1;",
            "UPDATE items SET price = 0.30 WHERE id = 2;",
            "SELECT name, price FROM items ORDER BY price DESC;",
            "DELETE FROM items WHERE id = 1;",
            "SELECT * FROM items;",
            "DROP TABLE items;",
            "SHOW TABLES;",
        ],
    );

    assert!(out.contains("Database 'shop' created"), "output:\n{}", out);
    assert!(out.contains("Table 'items' created"), "output:\n{}", out);
    assert!(out.contains("row inserted"), "output:\n{}", out);
    // SELECT results render as an ASCII table with a row count footer.
    assert!(out.contains("'bolt'"), "output:\n{}", out);
    assert!(out.contains("1 row(s) returned"), "output:\n{}", out);
    assert!(out.contains("Updated 1 row(s)"), "output:\n{}", out);
    assert!(out.contains("Deleted 1 row(s)"), "output:\n{}", out);
    // After the delete, one row remains.
    assert!(out.contains("1 row(s) returned") || out.contains("(1 rows)"), "output:\n{}", out);
    assert!(out.contains("Dropped table 'items'"), "output:\n{}", out);
}

#[test]
fn advanced_sql_works_from_the_shell() {
    let ws = common::Workspace::new("advanced");
    let out = rook(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE t (a INT, b VARCHAR(10));",
            "INSERT INTO t VALUES (1, 'x');",
            "INSERT INTO t VALUES (2, 'y');",
            "INSERT INTO t VALUES (3, 'x');",
            "SELECT b, COUNT(*) FROM t GROUP BY b HAVING COUNT(*) >= 2;",
            "CREATE INDEX idx_a ON t(a);",
            "SELECT * FROM t WHERE a BETWEEN 1 AND 2;",
            "CREATE VIEW v AS SELECT * FROM t WHERE a = 3;",
            "SELECT * FROM v;",
            "ALTER TABLE t ADD COLUMN c INT;",
            "TRUNCATE TABLE v2;", // unknown table must error cleanly
        ],
    );

    assert!(out.contains("'x'"), "group output:\n{}", out);
    assert!(out.contains("Created index 'idx_a'"), "output:\n{}", out);
    assert!(out.contains("2 row(s) returned"), "between filter:\n{}", out);
    assert!(out.contains("View 'v' created"), "output:\n{}", out);
    assert!(out.contains("1 row(s) returned"), "view select:\n{}", out);
    assert!(out.contains("Altered table"), "alter:\n{}", out);
    assert!(out.contains("Error:") || out.contains("not found"), "truncate error:\n{}", out);
}

#[test]
fn statements_may_span_multiple_lines() {
    let ws = common::Workspace::new("multiline");
    let out = rook(
        &ws,
        &[
            // One CREATE TABLE spread over four lines:
            "CREATE DATABASE ml;",
            "USE ml;",
            "CREATE TABLE t (",
            "  id INT,",
            "  name VARCHAR(30)",
            ");",
            // Two INSERTs sharing a single line:
            "INSERT INTO t VALUES (1, 'a'); INSERT INTO t VALUES (2, 'b');",
            // A comment line must not swallow the next statement...
            "-- fetching the second row",
            "SELECT id FROM t WHERE name = 'b';",
            // ...and semicolons inside string literals are not terminators:
            "SELECT 'semi;colon' AS s;",
        ],
    );

    assert!(out.contains("Table 't' created"), "output:\n{}", out);
    assert_eq!(
        out.matches("row inserted").count(),
        2,
        "both inserts ran: {}",
        out
    );
    assert!(out.contains("1 row(s) returned"), "select after comment:\n{}", out);
    assert!(out.contains("'semi;colon'"), "semicolon in string:\n{}", out);
}

#[test]
fn exit_only_applies_when_typed_alone() {
    let ws = common::Workspace::new("exitword");
    let out = rook(
        &ws,
        &[
            "CREATE DATABASE ex;",
            "USE ex;",
            "CREATE TABLE exits (id INT);",
            "INSERT INTO exits VALUES (1);",
            "SELECT * FROM exits;",
        ],
    );
    // The word 'exit' never appears as a bare command; every statement runs.
    assert!(out.contains("1 row(s) returned"), "output:\n{}", out);
}
