//! Regressions for defects found in the architecture review.
//!
//! Each test reproduces one finding end to end through the CLI: an index that
//! lost rows, a constraint that was never enforced, a clause that was dropped
//! instead of rejected, or a join that bound the wrong column.

mod common;

use common::{Workspace, expect_rows, run};

/// `n` rows `(i, i % modulus)` as multi-row INSERT statements.
fn numbered_rows(table: &str, n: usize, modulus: usize) -> Vec<String> {
    (1..=n)
        .collect::<Vec<_>>()
        .chunks(200)
        .map(|chunk| {
            let values: Vec<String> = chunk
                .iter()
                .map(|i| format!("({}, {})", i, i % modulus))
                .collect();
            format!("INSERT INTO {} VALUES {};", table, values.join(", "))
        })
        .collect()
}

fn run_owned(ws: &Workspace, lines: &[String]) -> String {
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    run(ws, &refs)
}

// ── B-tree: duplicates of one key spanning several leaves ───────────────────

#[test]
fn non_unique_index_finds_every_duplicate() {
    let ws = Workspace::new("rf_dupes");
    let mut setup = vec![
        "CREATE DATABASE d;".to_string(),
        "USE d;".to_string(),
        "CREATE TABLE i (id INT, k INT);".to_string(),
        "CREATE INDEX i_k ON i (k);".to_string(),
    ];
    // 3,000 rows over 3 keys: each key's entries span several index leaves.
    setup.extend(numbered_rows("i", 3000, 3));
    run_owned(&ws, &setup);

    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM i WHERE k = 1;",
        &[&["1000"]],
    );

    // DELETE through the same index must remove all of them too.
    let out = run(&ws, &["USE d;", "DELETE FROM i WHERE k = 1;"]);
    assert!(out.contains("Deleted 1000 row(s)"), "{}", out);
    expect_rows(&ws, &["USE d;"], "SELECT COUNT(*) FROM i;", &[&["2000"]]);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM i WHERE k = 2;",
        &[&["1000"]],
    );
}

#[test]
fn index_join_returns_every_match() {
    let ws = Workspace::new("rf_inlj");
    let mut setup = vec![
        "CREATE DATABASE d;".to_string(),
        "USE d;".to_string(),
        "CREATE TABLE o (id INT, k INT);".to_string(),
        "CREATE TABLE i (id INT, k INT);".to_string(),
        "CREATE INDEX i_k ON i (k);".to_string(),
        "INSERT INTO o VALUES (1, 1), (2, 2), (3, NULL);".to_string(),
    ];
    setup.extend(numbered_rows("i", 3000, 3));
    run_owned(&ws, &setup);

    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM o JOIN i ON o.k = i.k;",
        &[&["2000"]],
    );
    // The unmatched NULL-key row survives a LEFT JOIN.
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM o LEFT JOIN i ON o.k = i.k;",
        &[&["2001"]],
    );
}

#[test]
fn index_with_large_keys_keeps_every_entry() {
    let ws = Workspace::new("rf_bigkeys");
    let mut setup = vec![
        "CREATE DATABASE d;".to_string(),
        "USE d;".to_string(),
        "CREATE TABLE bk (id INT, k VARCHAR(4000));".to_string(),
        "CREATE INDEX bk_k ON bk (k);".to_string(),
    ];
    for i in 1..=300 {
        setup.push(format!("INSERT INTO bk VALUES ({}, 'a{:04}');", i, i));
    }
    // 2.5 KB keys: three fill a leaf, so splits must balance by bytes.
    for i in 0..8 {
        setup.push(format!(
            "INSERT INTO bk VALUES ({}, 'y{}{}');",
            900 + i,
            i,
            "z".repeat(2500)
        ));
    }
    run_owned(&ws, &setup);

    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM bk WHERE k >= 'y' AND k < 'z';",
        &[&["8"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM bk WHERE k >= 'a0001' AND k <= 'a0300';",
        &[&["300"]],
    );
}

// ── Joins ────────────────────────────────────────────────────────────────────

const JOIN_SETUP: &[&str] = &[
    "CREATE DATABASE d;",
    "USE d;",
    "CREATE TABLE a (id INT, x INT, k INT);",
    "INSERT INTO a VALUES (1, 10, 1), (2, 20, 1), (3, NULL, 2), (4, 20, 2);",
    "CREATE TABLE r (k INT, v VARCHAR(5));",
    "INSERT INTO r VALUES (1, 'one'), (2, 'two'), (3, 'three');",
];

#[test]
fn index_join_reads_the_key_from_the_table_named_in_on() {
    let ws = Workspace::new("rf_inlj_bind");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE o (id INT, k INT);",
            "CREATE TABLE ib (id INT, k BIGINT);",
            "CREATE INDEX ib_k ON ib (k);",
            "INSERT INTO o VALUES (1, 1), (2, 2), (3, NULL);",
            "INSERT INTO ib VALUES (10, 1), (11, 2);",
        ],
    );
    // `b.k`, not `a.k`: both exist in the left input.
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT a.id, b.id, ib.id FROM o a JOIN o b ON a.id = b.id + 1 \
         JOIN ib ON b.k = ib.k ORDER BY a.id;",
        &[&["2", "1", "10"], &["3", "2", "11"]],
    );
}

#[test]
fn comma_from_list_is_a_cross_join() {
    let ws = Workspace::new("rf_comma");
    run(&ws, JOIN_SETUP);
    expect_rows(&ws, &["USE d;"], "SELECT COUNT(*) FROM a, r;", &[&["12"]]);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT a.id, r.v FROM a, r WHERE a.k = r.k AND a.id > 2 ORDER BY a.id;",
        &[&["3", "'two'"], &["4", "'two'"]],
    );
}

#[test]
fn where_clause_keeps_table_qualifiers_across_a_join() {
    let ws = Workspace::new("rf_qual");
    run(&ws, JOIN_SETUP);
    // `a.k = r.k` used to be pushed to `a` alone and compare `k` with itself.
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT a.id, r.v FROM a JOIN r ON a.id > 0 WHERE a.k = r.k AND a.id > 2 ORDER BY a.id;",
        &[&["3", "'two'"], &["4", "'two'"]],
    );
}

#[test]
fn join_using_is_an_equi_join() {
    let ws = Workspace::new("rf_using");
    run(&ws, JOIN_SETUP);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM a JOIN r USING (k);",
        &[&["4"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT r.v, a.id FROM r LEFT JOIN a USING (k) ORDER BY r.k, a.id;",
        &[
            &["'one'", "1"],
            &["'one'", "2"],
            &["'two'", "3"],
            &["'two'", "4"],
            &["'three'", "NULL"],
        ],
    );
}

#[test]
fn outer_equi_joins_match_nested_loop_results() {
    let ws = Workspace::new("rf_outer");
    run(&ws, JOIN_SETUP);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT a.id, r.v FROM a RIGHT JOIN r ON a.k = r.k AND a.id > 1 ORDER BY v, id;",
        &[
            &["2", "'one'"],
            &["NULL", "'three'"],
            &["3", "'two'"],
            &["4", "'two'"],
        ],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM a FULL JOIN r ON a.k = r.k;",
        &[&["5"]],
    );
}

// ── Aggregates ───────────────────────────────────────────────────────────────

#[test]
fn aggregates_of_the_same_function_stay_separate() {
    let ws = Workspace::new("rf_aggs");
    run(&ws, JOIN_SETUP);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT SUM(x), SUM(id), COUNT(*), COUNT(x), COUNT(DISTINCT x) FROM a;",
        &[&["50", "10", "4", "3", "2"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT k, SUM(x) + SUM(id), MAX(x) - MIN(id) FROM a GROUP BY k ORDER BY k;",
        &[&["1", "33", "19"], &["2", "27", "17"]],
    );
}

#[test]
fn having_distinguishes_distinct_aggregates() {
    let ws = Workspace::new("rf_having");
    run(&ws, JOIN_SETUP);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT k, COUNT(x) AS c FROM a GROUP BY k HAVING COUNT(DISTINCT x) = 1;",
        &[&["2", "1"]],
    );
}

// ── Clauses that used to be dropped ──────────────────────────────────────────

#[test]
fn untranslated_clauses_are_rejected() {
    let ws = Workspace::new("rf_reject");
    run(&ws, JOIN_SETUP);
    for (sql, needle) in [
        ("SELECT SUM(id) FILTER (WHERE id > 1) FROM a;", "FILTER"),
        ("SELECT DISTINCT ON (k) id FROM a;", "DISTINCT ON"),
        (
            "SELECT id FROM a ORDER BY id FETCH FIRST 1 ROWS ONLY;",
            "FETCH",
        ),
        ("SELECT ROW_NUMBER() OVER (ORDER BY id) FROM a;", "Window"),
    ] {
        let out = run(&ws, &["USE d;", sql]);
        assert!(
            out.contains("Parse error") && out.contains(needle),
            "{} should be rejected:\n{}",
            sql,
            out
        );
    }
}

// ── Recursive CTEs ───────────────────────────────────────────────────────────

#[test]
fn recursive_union_removes_duplicates_and_reports_runaway_recursion() {
    let ws = Workspace::new("rf_cte");
    run(&ws, JOIN_SETUP);
    // The anchor yields k = 1, 1, 2, 2; UNION keeps one of each.
    expect_rows(
        &ws,
        &["USE d;"],
        "WITH RECURSIVE c AS (SELECT k AS n FROM a UNION SELECT n FROM c) \
         SELECT n FROM c ORDER BY n;",
        &[&["1"], &["2"]],
    );
    let out = run(
        &ws,
        &[
            "USE d;",
            "WITH RECURSIVE c AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM c WHERE n < 12000) \
             SELECT COUNT(*) FROM c;",
        ],
    );
    assert!(out.contains("did not terminate"), "{}", out);
}

// ── Constraints and DDL ──────────────────────────────────────────────────────

#[test]
fn check_constraints_are_enforced() {
    let ws = Workspace::new("rf_check");
    let out = run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE k1 (a INT CHECK (a > 0), b INT);",
            "CREATE TABLE k2 (a INT, b INT, CHECK (a > 0));",
            "INSERT INTO k1 VALUES (-1, 1);",
            "INSERT INTO k2 VALUES (-1, 1);",
            "INSERT INTO k1 VALUES (5, 1);",
            "INSERT INTO k2 VALUES (5, 1);",
            // The stored expression follows a column rename.
            "ALTER TABLE k1 RENAME COLUMN a TO aa;",
            "INSERT INTO k1 VALUES (-2, 2);",
            "INSERT INTO k1 VALUES (6, 2);",
        ],
    );
    assert_eq!(
        out.matches("CHECK constraint violated").count(),
        3,
        "{}",
        out
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT aa FROM k1 ORDER BY aa;",
        &[&["5"], &["6"]],
    );
    expect_rows(&ws, &["USE d;"], "SELECT a FROM k2;", &[&["5"]]);
}

#[test]
fn create_table_rejects_a_bad_definition_whole() {
    let ws = Workspace::new("rf_create");
    let out = run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE bad (id INT, z NOSUCHTYPE, w INT);",
            "CREATE TABLE bad2 (id INT, q INT DEFAULT 'zz');",
            "CREATE TABLE ok (id INT, name VARCHAR(10) DEFAULT ('abc'));",
            "INSERT INTO ok (id) VALUES (1);",
            "SELECT * FROM bad;",
            "SELECT * FROM bad2;",
        ],
    );
    assert!(out.contains("Table 'bad' was not created"), "{}", out);
    assert!(out.contains("Table 'bad2' was not created"), "{}", out);
    assert!(out.contains("Table 'bad' not found"), "{}", out);
    assert!(out.contains("Table 'bad2' not found"), "{}", out);
    expect_rows(&ws, &["USE d;"], "SELECT name FROM ok;", &[&["'abc'"]]);
}

#[test]
fn add_column_applies_not_null_and_default() {
    let ws = Workspace::new("rf_addcol");
    let out = run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE t (id INT);",
            "INSERT INTO t VALUES (1), (2);",
            "ALTER TABLE t ADD COLUMN z INT NOT NULL DEFAULT 7;",
            "INSERT INTO t (id) VALUES (3);",
            "ALTER TABLE t ADD COLUMN y INT NOT NULL;",
        ],
    );
    assert!(out.contains("Cannot add NOT NULL column 'y'"), "{}", out);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT id, z FROM t ORDER BY id;",
        &[&["1", "7"], &["2", "7"], &["3", "7"]],
    );
}

#[test]
fn delete_blocked_by_a_foreign_key_changes_nothing() {
    let ws = Workspace::new("rf_fkdel");
    let out = run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE p (id INT PRIMARY KEY);",
            "CREATE TABLE c1 (id INT, pid INT, FOREIGN KEY (pid) REFERENCES p(id) ON DELETE CASCADE);",
            "CREATE TABLE c2 (id INT, pid INT REFERENCES p(id));",
            "INSERT INTO p VALUES (1), (2), (3);",
            "INSERT INTO c1 VALUES (100, 1), (101, 2);",
            "INSERT INTO c2 VALUES (200, 1);",
            // Row 1 is restricted by c2: nothing may be deleted, and the
            // cascade into c1 must not have run.
            "DELETE FROM p;",
            "TRUNCATE TABLE p;",
            "ALTER TABLE p RENAME TO p2;",
        ],
    );
    assert!(out.contains("Delete failed"), "{}", out);
    assert!(out.contains("Cannot truncate table 'p'"), "{}", out);
    assert!(out.contains("Cannot rename table 'p'"), "{}", out);
    expect_rows(&ws, &["USE d;"], "SELECT COUNT(*) FROM p;", &[&["3"]]);
    expect_rows(&ws, &["USE d;"], "SELECT COUNT(*) FROM c1;", &[&["2"]]);

    // Unrestricted rows still delete, cascading into c1.
    let out = run(&ws, &["USE d;", "DELETE FROM p WHERE id > 1;"]);
    assert!(out.contains("Deleted 2 row(s)"), "{}", out);
    expect_rows(&ws, &["USE d;"], "SELECT id FROM c1;", &[&["100"]]);
}

#[test]
fn create_table_as_select_types_an_empty_result() {
    let ws = Workspace::new("rf_ctas");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE src (a INT, s VARCHAR(20));",
            "INSERT INTO src VALUES (1, 'x');",
            "CREATE TABLE c2 AS SELECT a, s FROM src WHERE a > 100;",
            "INSERT INTO c2 VALUES (5, 'y');",
        ],
    );
    // `a` is an INT (unquoted), not VARCHAR(255).
    expect_rows(&ws, &["USE d;"], "SELECT a, s FROM c2;", &[&["5", "'y'"]]);
}
