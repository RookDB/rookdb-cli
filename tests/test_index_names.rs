//! Index names are unique per database: DROP INDEX finds an index by name
//! alone, so a second `CREATE INDEX` with a name in use must fail instead of
//! overwriting the first index's file and adding an ambiguous row.

mod common;

use common::{Workspace, expect_rows, run};

const SETUP: &[&str] = &[
    "CREATE DATABASE d;",
    "USE d;",
    "CREATE TABLE t (a INT, b INT);",
    "CREATE TABLE u (a INT, b INT);",
    "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30);",
];

fn with(extra: &[&'static str]) -> Vec<&'static str> {
    SETUP.iter().chain(extra.iter()).copied().collect()
}

const INDEX_NAMES: &str = "SELECT index_name FROM information_schema.indexes ORDER BY index_name;";

#[test]
fn second_index_with_the_same_name_is_rejected() {
    let ws = Workspace::new("ix_dup");
    let out = run(
        &ws,
        &with(&[
            "CREATE INDEX same_name ON t (a);",
            "CREATE INDEX same_name ON t (b);",
        ]),
    );
    assert!(out.contains("Index 'same_name' already exists"), "{}", out);

    // One index remains, and it is still the one on `a`.
    expect_rows(&ws, &["USE d;"], INDEX_NAMES, &[&["'same_name'"]]);
    expect_rows(&ws, &["USE d;"], "SELECT b FROM t WHERE a = 2;", &[&["20"]]);

    // The name is also taken on another table, and case does not matter.
    let out = run(
        &ws,
        &[
            "USE d;",
            "CREATE INDEX same_name ON u (a);",
            "CREATE INDEX \"SAME_name\" ON u (b);",
        ],
    );
    assert_eq!(
        out.matches("Index 'same_name' already exists").count()
            + out.matches("Index 'SAME_name' already exists").count(),
        2,
        "{}",
        out
    );
    expect_rows(&ws, &["USE d;"], INDEX_NAMES, &[&["'same_name'"]]);
}

#[test]
fn if_not_exists_skips_a_name_in_use_and_drop_frees_it() {
    let ws = Workspace::new("ix_ine");
    let out = run(
        &ws,
        &with(&[
            "CREATE INDEX ix ON t (a);",
            "CREATE INDEX IF NOT EXISTS ix ON t (b);",
        ]),
    );
    assert!(!out.contains("already exists\n"), "{}", out);
    assert!(out.contains("IF NOT EXISTS specified, skipping"), "{}", out);
    expect_rows(&ws, &["USE d;"], INDEX_NAMES, &[&["'ix'"]]);

    run(&ws, &["USE d;", "DROP INDEX ix;"]);
    expect_rows(&ws, &["USE d;"], INDEX_NAMES, &[]);
    run(&ws, &["USE d;", "CREATE INDEX ix ON t (b);"]);
    expect_rows(&ws, &["USE d;"], INDEX_NAMES, &[&["'ix'"]]);
}

#[test]
fn constraint_indexes_and_user_names_do_not_collide() {
    let ws = Workspace::new("ix_constraint");
    // A user name that the PRIMARY KEY's automatic index would take.
    let out = run(
        &ws,
        &with(&[
            "CREATE INDEX pk_p_id ON t (b);",
            "CREATE TABLE p (id INT PRIMARY KEY, v INT);",
        ]),
    );
    assert!(!out.contains("Warning"), "{}", out);
    let names = run(&ws, &["USE d;", INDEX_NAMES]);
    assert_eq!(names.matches("pk_p_id").count(), 2, "{}", names);

    // The automatic index's name is taken like any other.
    let out = run(&ws, &["USE d;", "CREATE INDEX pk_p_id_2 ON p (v);"]);
    assert!(out.contains("Index 'pk_p_id_2' already exists"), "{}", out);

    // DROP INDEX removes only the user's index; the key is still enforced.
    run(&ws, &["USE d;", "DROP INDEX pk_p_id;"]);
    let out = run(
        &ws,
        &[
            "USE d;",
            "INSERT INTO p VALUES (1, 1);",
            "INSERT INTO p VALUES (1, 2);",
        ],
    );
    assert!(
        out.to_lowercase().contains("duplicate")
            || out.to_lowercase().contains("unique")
            || out.to_lowercase().contains("primary"),
        "{}",
        out
    );
    expect_rows(&ws, &["USE d;"], "SELECT COUNT(*) FROM p;", &[&["1"]]);
}
