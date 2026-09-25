//! Capability tests for the rookdb shell.
//!
//! Every test verifies **actual result values**: mutations are followed by a
//! SELECT whose box-drawing table is parsed into cells and compared exactly —
//! values, column count, row order and NULL rendering included. After every
//! UPDATE/DELETE the FULL table state is re-checked, so collateral damage to
//! untouched rows cannot hide.
//!
//! Cell strings must match the engine's rendering: string literals carry
//! single quotes (`'bolt'`), NULL is literal `NULL`, floats print their
//! shortest form (`0.3`), ints are plain.

mod common;

use common::{Workspace, expect_rows, run};

/// items(id, name, price) with three rows.
const SETUP: &[&str] = &[
    "CREATE DATABASE shop;",
    "USE shop;",
    "CREATE TABLE items (id INT, name VARCHAR(30), price DOUBLE PRECISION);",
    "INSERT INTO items VALUES (1, 'bolt', 0.25);",
    "INSERT INTO items VALUES (2, 'nut', 0.10);",
    "INSERT INTO items VALUES (3, 'washer', 1.50);",
];

fn shop_ws(name: &str) -> Workspace {
    let ws = Workspace::new(name);
    let out = run(&ws, SETUP);
    assert!(
        !out.to_lowercase().contains("error"),
        "setup failed:\n{}",
        out
    );
    ws
}

/// Run a mutating statement and fail the test if it reports an error.
#[track_caller]
fn exec_use(ws: &Workspace, stmt: &str) {
    let out = run(ws, &["USE shop;", stmt]);
    assert!(
        !out.to_lowercase().contains("error"),
        "`{}` reported an error:\n{}",
        stmt,
        out
    );
}

// ── CRUD with full-state verification ────────────────────────────────────────

#[test]
fn crud_full_state_tracking() {
    let ws = common::Workspace::new("crud");
    let out = run(&ws, SETUP);
    assert!(!out.to_lowercase().contains("error"), "{}", out);

    // Insertion order preserved; float rendering is the shortest form.
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT * FROM items;",
        &[
            &["1", "'bolt'", "0.25"],
            &["2", "'nut'", "0.1"],
            &["3", "'washer'", "1.5"],
        ],
    );

    // UPDATE must touch ONLY the matched row — the other two rows are part
    // of the expectation and would fail if anything else changed.
    exec_use(
        &ws,
        "UPDATE items SET name = 'hex nut', price = 0.30 WHERE id = 2;",
    );
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT * FROM items;",
        &[
            &["1", "'bolt'", "0.25"],
            &["2", "'hex nut'", "0.3"],
            &["3", "'washer'", "1.5"],
        ],
    );

    exec_use(&ws, "DELETE FROM items WHERE id = 1;");
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT * FROM items;",
        &[&["2", "'hex nut'", "0.3"], &["3", "'washer'", "1.5"]],
    );

    // Deleting a row that no longer changes nothing.
    exec_use(&ws, "DELETE FROM items WHERE id = 99;");
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT * FROM items;",
        &[&["2", "'hex nut'", "0.3"], &["3", "'washer'", "1.5"]],
    );
}

// ── Projections, predicates, ordering ────────────────────────────────────────

#[test]
fn projection_where_order_values() {
    let ws = shop_ws("proj");

    // Projection keeps only the requested column's values.
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT name FROM items WHERE price >= 0.25 ORDER BY price;",
        &[&["'bolt'"], &["'washer'"]],
    );

    // Equality predicate on a string column.
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT id, name FROM items WHERE name = 'washer';",
        &[&["3", "'washer'"]],
    );

    // BETWEEN bounds are inclusive.
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT id FROM items WHERE price BETWEEN 0.10 AND 0.30 ORDER BY price;",
        &[&["2"], &["1"]],
    );

    // Arithmetic projection evaluates per row.
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT id, price * 2 FROM items WHERE id = 3;",
        &[&["3", "3"]],
    );
}

// ── ORDER BY / LIMIT / OFFSET windows ────────────────────────────────────────

#[test]
fn order_limit_offset_windows_are_exact() {
    let ws = common::Workspace::new("windows");

    let out = run(
        &ws,
        &[
            "CREATE DATABASE win;",
            "USE win;",
            "CREATE TABLE emp (id INT, name VARCHAR(20), salary INT);",
            "INSERT INTO emp VALUES (1, 'Ann', 50000);",
            "INSERT INTO emp VALUES (2, 'Ben', 62000);",
            "INSERT INTO emp VALUES (3, 'Cy', 75000);",
            "INSERT INTO emp VALUES (4, 'Dee', 91000);",
            "INSERT INTO emp VALUES (5, 'Eli', 48000);",
        ],
    );
    assert!(!out.to_lowercase().contains("error"), "{}", out);

    const PRE: &[&str] = &["USE win;"];

    // DESC gives the exact salary ranking.
    expect_rows(
        &ws,
        PRE,
        "SELECT name FROM emp ORDER BY salary DESC;",
        &[&["'Dee'"], &["'Cy'"], &["'Ben'"], &["'Ann'"], &["'Eli'"]],
    );

    // LIMIT/OFFSET windows slice the ordered stream precisely.
    expect_rows(
        &ws,
        PRE,
        "SELECT id FROM emp ORDER BY salary DESC LIMIT 2 OFFSET 1;",
        &[&["3"], &["2"]],
    );

    // OFFSET beyond the end yields an empty result.
    expect_rows(
        &ws,
        PRE,
        "SELECT id FROM emp ORDER BY salary DESC LIMIT 3 OFFSET 5;",
        &[],
    );

    // LIMIT 0 emits zero rows (regression guard for the LIMIT 0 fix).
    expect_rows(
        &ws,
        PRE,
        "SELECT id FROM emp ORDER BY salary ASC LIMIT 0;",
        &[],
    );
}

// ── DDL lifecycle reflected in values ────────────────────────────────────────

#[test]
fn ddl_lifecycle_values() {
    let ws = shop_ws("ddl");

    // Added column reads back as NULL for every existing row.
    exec_use(&ws, "ALTER TABLE items ADD COLUMN stock INT;");
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT id, stock FROM items;",
        &[&["1", "NULL"], &["2", "NULL"], &["3", "NULL"]],
    );

    // Setting stock on one row leaves the other NULLs intact.
    exec_use(&ws, "UPDATE items SET stock = 42 WHERE id = 2;");
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT id, stock FROM items;",
        &[&["1", "NULL"], &["2", "42"], &["3", "NULL"]],
    );

    // DROP removes the table entirely.
    exec_use(&ws, "DROP TABLE items;");
    let out = run(&ws, &["USE shop;", "SELECT * FROM items;"]);
    assert!(
        out.contains("does not exist") || out.contains("not found"),
        "expected a not-found error:\n{}",
        out
    );
}

// ── Indexes and views return correct DATA through their paths ────────────────

#[test]
fn indexes_and_views_return_correct_values() {
    let ws = shop_ws("iv");

    // Index-driven lookup must return the same VALUES a sequential scan would.
    exec_use(&ws, "CREATE INDEX by_price ON items(price);");
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT name, price FROM items WHERE price = 1.5;",
        &[&["'washer'", "1.5"]],
    );
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT name FROM items WHERE price <= 0.25 ORDER BY price;",
        &[&["'nut'"], &["'bolt'"]],
    );

    // A view stores its query and replays it with correct filtering.
    exec_use(
        &ws,
        "CREATE VIEW pricey AS SELECT id, name FROM items WHERE price >= 0.25;",
    );
    expect_rows(
        &ws,
        &["USE shop;"],
        "SELECT * FROM pricey;",
        &[&["1", "'bolt'"], &["3", "'washer'"]],
    );

    exec_use(&ws, "DROP VIEW pricey;");
    let out = run(&ws, &["USE shop;", "SELECT * FROM pricey;"]);
    assert!(out.contains("does not exist") || out.contains("not found"));
}

// ── Multi-line input, comments, embedded semicolons ─────────────────────────

#[test]
fn multiline_input_yields_exact_results() {
    let ws = common::Workspace::new("multiline");

    let out = run(
        &ws,
        &[
            "CREATE DATABASE ml;",
            "USE ml;",
            // Statement spanning multiple lines:
            "CREATE TABLE notes (",
            "  id INT,",
            "  note VARCHAR(40)",
            ");",
            // Two INSERTs sharing one physical line:
            "INSERT INTO notes VALUES (1, 'first'); INSERT INTO notes VALUES (2, 'second');",
            // Comment line must not swallow the next statement:
            "-- fetching everything",
            "SELECT * FROM notes ORDER BY id;",
            // Semicolon INSIDE a string literal is not a terminator:
            "INSERT INTO notes VALUES (3, 'semi;colon');",
            "SELECT note FROM notes WHERE id = 3;",
        ],
    );

    let table = common::parse_last_table(&out);
    assert_eq!(
        table.column("note"),
        vec!["'semi;colon'"],
        "comment handling / embedded semicolon broke results:\n{}",
        out
    );
    assert_eq!(
        common::parse_tables(&out).len(),
        2,
        "both SELECTs must produce tables:\n{}",
        out
    );
}

// ── Errors never kill the session ────────────────────────────────────────────

#[test]
fn session_survives_errors() {
    let ws = shop_ws("err");

    let out = run(
        &ws,
        &[
            "USE shop;",
            "SELECT * FROM missing_table;", // runtime error
            "SELEC bogus;",                 // parse error
            "SELECT id FROM items WHERE id = 1;",
        ],
    );

    // The final valid statement still executes after two failures.
    let table = common::parse_last_table(&out);
    assert_eq!(table.rows, vec![vec!["1"]], "output:\n{}", out);
}
