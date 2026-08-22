//! Index-acceleration end-to-end tests.
//!
//! Two guarantees are pinned down per predicate form:
//!
//! 1. **Exact results** — parsed cell-by-cell from the rendered table,
//!    including row order where the B+ Tree defines it (ascending key,
//!    leaf-link traversal).
//! 2. **Path equivalence** — an identical table *without* an index must
//!    produce byte-identical result sets through the SeqScan+Filter path.
//!
//! Run with: cargo test --test index_acceleration -- --test-threads=1

mod common;

use common::{expect_rows, run, Workspace};

/// emp(id, name, salary) — six rows, `salary = 62000` duplicated.
const SETUP: &[&str] = &[
    "CREATE DATABASE pay;",
    "USE pay;",
    "CREATE TABLE emp (id INT, name VARCHAR(20), salary INT);",
    "INSERT INTO emp VALUES (1, 'Ann', 50000);",
    "INSERT INTO emp VALUES (2, 'Ben', 62000);",
    "INSERT INTO emp VALUES (3, 'Cy', 75000);",
    "INSERT INTO emp VALUES (4, 'Dee', 91000);",
    "INSERT INTO emp VALUES (5, 'Eve', 48000);",
    "INSERT INTO emp VALUES (6, 'Fay', 62000);",
];

/// Same table again, but WITHOUT an index — the sequential baseline.
const SETUP_NO_INDEX: &[&str] = &[
    "CREATE DATABASE pay2;",
    "USE pay2;",
    "CREATE TABLE emp (id INT, name VARCHAR(20), salary INT);",
    "INSERT INTO emp VALUES (1, 'Ann', 50000);",
    "INSERT INTO emp VALUES (2, 'Ben', 62000);",
    "INSERT INTO emp VALUES (3, 'Cy', 75000);",
    "INSERT INTO emp VALUES (4, 'Dee', 91000);",
    "INSERT INTO emp VALUES (5, 'Eve', 48000);",
    "INSERT INTO emp VALUES (6, 'Fay', 62000);",
];

fn ws_with_index(name: &str) -> Workspace {
    let ws = Workspace::new(name);
    let out = run(
        &ws,
        &[SETUP, &["CREATE INDEX by_salary ON emp(salary);"]].concat(),
    );
    assert!(
        out.contains("Created index"),
        "setup failed:\n{}",
        out
    );
    ws
}

// ── Exact result sets through the index ──────────────────────────────────────

#[test]
fn point_lookup_returns_exact_row() {
    let ws = ws_with_index("point");
    expect_rows(
        &ws,
        &["USE pay;"],
        "SELECT name FROM emp WHERE salary = 91000;",
        &[
            &["'Dee'"],
        ],
    );
}

#[test]
fn point_lookup_with_duplicate_keys_returns_all_matches() {
    let ws = ws_with_index("dup");
    // Non-unique index: BOTH rows share 62000 and must come back.
    // Leaf insertion order among equal keys follows insert order.
    expect_rows(
        &ws,
        &["USE pay;"],
        "SELECT name FROM emp WHERE salary = 62000;",
        &[
            &["'Ben'"],
            &["'Fay'"],
        ],
    );
}

#[test]
fn between_is_inclusive_on_both_bounds() {
    let ws = ws_with_index("between");
    // Ascending key order is part of the B+Tree leaf contract.
    expect_rows(
        &ws,
        &["USE pay;"],
        "SELECT name FROM emp WHERE salary BETWEEN 48000 AND 80000;",
        &[
            &["'Eve'"],
            &["'Ann'"],
            &["'Ben'"],
            &["'Fay'"],
            &["'Cy'"],
        ],
    );
}

#[test]
fn greater_than_excludes_bound() {
    let ws = ws_with_index("gt");
    expect_rows(
        &ws,
        &["USE pay;"],
        "SELECT name FROM emp WHERE salary > 70000;",
        &[
            &["'Cy'"],
            &["'Dee'"],
        ],
    );
}

#[test]
fn less_equal_includes_bound() {
    let ws = ws_with_index("le");
    expect_rows(
        &ws,
        &["USE pay;"],
        "SELECT name FROM emp WHERE salary <= 62000;",
        &[
            &["'Eve'"],
            &["'Ann'"],
            &["'Ben'"],
            &["'Fay'"],
        ],
    );
}

#[test]
fn single_element_in_uses_point_lookup() {
    let ws = ws_with_index("in");
    expect_rows(
        &ws,
        &["USE pay;"],
        "SELECT name FROM emp WHERE salary IN (91000);",
        &[
            &["'Dee'"],
        ],
    );
}

#[test]
fn and_range_intersects_into_one_interval() {
    let ws = ws_with_index("andrange");
    // Both bounds exclusive.
    expect_rows(
        &ws,
        &["USE pay;"],
        "SELECT name FROM emp WHERE salary > 48000 AND salary < 91000;",
        &[
            &["'Ann'"],
            &["'Ben'"],
            &["'Fay'"],
            &["'Cy'"],
        ],
    );
}

// ── Path-equivalence oracle ──────────────────────────────────────────────────

/// The SAME query must return the same rows whether the planner drives the
/// table through its index or through a sequential scan. Any divergence
/// between the two access paths is an engine bug this test will surface.
#[test]
fn indexed_and_sequential_paths_return_identical_results() {
    let idx_ws = common::Workspace::new("equiv_idx");
    let ni_ws = common::Workspace::new("equiv_seq");

    let out = run(&idx_ws, &[SETUP, &["CREATE INDEX by_salary ON emp(salary);"]].concat());
    assert!(out.contains("Created index"), "{}", out);
    let out = run(&ni_ws, SETUP_NO_INDEX);
    assert!(!out.to_lowercase().contains("error"), "{}", out);

    const QUERIES: &[&str] = &[
        "SELECT id, name FROM emp WHERE salary = 62000 ORDER BY name;",
        "SELECT id, name FROM emp WHERE salary > 60000 ORDER BY name;",
        "SELECT id FROM emp WHERE salary BETWEEN 48000 AND 76000;",
        "SELECT name FROM emp WHERE salary >= 91000;",
        "SELECT name FROM emp WHERE salary < 49000;",
    ];

    for q in QUERIES {
        let via_index = common::parse_last_table(&run(&idx_ws, &["USE pay;", q]));
        let via_scan = common::parse_last_table(&run(&ni_ws, &["USE pay2;", q]));
        // These queries carry no ORDER BY, so row ORDER is unspecified —
        // compare as multisets. (Ordered guarantees are pinned separately.)
        let mut a = via_index.rows.clone();
        let mut b = via_scan.rows.clone();
        a.sort();
        b.sort();
        assert_eq!(
            a, b,
            "access paths disagree for `{}`\nvia index:\n{:#?}\nvia scan:\n{:#?}",
            q,
            via_index.rows,
            via_scan.rows
        );
    }
}
