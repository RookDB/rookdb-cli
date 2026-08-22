//! End-to-end smoke tests: run the `rookdb` binary as a subprocess and drive
//! a full CREATE → INSERT → SELECT → UPDATE → DELETE workflow.
//!
//! Each test gets an isolated workspace directory so tests never share state,
//! and the binary is located relative to CARGO_MANIFEST_DIR.

use std::io::Write;
use std::process::{Command, Stdio};

fn workspace(name: &str) -> String {
    let dir = format!(
        "{}/database_cli_smoke_{}_{}",
        env!("CARGO_MANIFEST_DIR"),
        std::process::id(),
        name
    );
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rookdb_bin() -> String {
    // CARGO_BIN_EXE_* is provided by cargo for integration tests and
    // guarantees the binary has been built before the test runs.
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
fn crud_workflow_via_typed_plans() {
    let ws = workspace("crud");
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
            "SELECT * FROM items WHERE id = 2;",
            "DELETE FROM items WHERE id = 1;",
            "SELECT * FROM items WHERE id = 1;",
        ],
    );

    assert!(out.contains("Database 'shop' created"), "output:\n{}", out);
    assert!(out.contains("Table 'items' created"), "output:\n{}", out);
    assert!(out.contains("1 row(s) inserted"), "output:\n{}", out);
    assert!(out.contains("name='bolt'") || out.contains("name=bolt"), "output:\n{}", out);
    assert!(out.contains("Updated 1 row(s)"), "output:\n{}", out);
    assert!(out.contains("Deleted 1 row(s)"), "output:\n{}", out);
    // After the delete, the row must be gone (found count 0).
    assert!(out.contains("(found 0)"), "output:\n{}", out);
}

#[test]
fn unsupported_statements_report_cleanly() {
    let ws = workspace("unsupported");
    let out = rook(
        &ws,
        &[
            "CREATE DATABASE d1;",
            "USE d1;",
            "CREATE TABLE t (a INT);",
            "DROP TABLE t;",
            "CREATE INDEX i1 ON t(a);",
        ],
    );
    assert!(out.contains("not executable yet"), "output:\n{}", out);
}
