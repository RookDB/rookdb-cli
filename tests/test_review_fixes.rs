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
        ],
    );
    assert!(out.contains("Delete failed"), "{}", out);
    assert!(out.contains("Cannot truncate table 'p'"), "{}", out);
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

// ── Expressions and ORDER BY ────────────────────────────────────────────────

#[test]
fn constant_and_boolean_predicates_evaluate() {
    let ws = Workspace::new("rf_constpred");
    run(&ws, JOIN_SETUP);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT id FROM a WHERE 1 = 1 ORDER BY id;",
        &[&["1"], &["2"], &["3"], &["4"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT id FROM a WHERE 2 > 1 AND k = 1 ORDER BY id;",
        &[&["1"], &["2"]],
    );
    expect_rows(&ws, &["USE d;"], "SELECT id FROM a WHERE 1 = 2;", &[]);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT id FROM a WHERE k = 2 OR FALSE ORDER BY id;",
        &[&["3"], &["4"]],
    );
}

#[test]
fn concat_operator_propagates_null_and_modulo_works() {
    let ws = Workspace::new("rf_concat");
    run(&ws, JOIN_SETUP);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT v || NULL, 'a' || v, k % 2 FROM r ORDER BY k;",
        &[
            &["NULL", "'aone'", "1"],
            &["NULL", "'atwo'", "0"],
            &["NULL", "'athree'", "1"],
        ],
    );
    let out = run(
        &ws,
        &["USE d;", "SELECT TRIM(BOTH 'x' FROM 'xxaxx') FROM r;"],
    );
    assert!(
        out.contains("Parse error") && out.contains("TRIM"),
        "{}",
        out
    );
}

#[test]
fn group_by_and_order_by_accept_select_expressions() {
    let ws = Workspace::new("rf_groupexpr");
    run(&ws, JOIN_SETUP);
    // x is 10, 20, NULL, 20.
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT x + 1, COUNT(*) FROM a GROUP BY x + 1 ORDER BY 2 DESC, 1;",
        &[&["21", "2"], &["NULL", "1"], &["11", "1"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT k, COUNT(x) FROM a GROUP BY k ORDER BY COUNT(x) DESC;",
        &[&["1", "2"], &["2", "1"]],
    );
}

#[test]
fn order_by_prefers_a_select_alias_over_an_input_column() {
    let ws = Workspace::new("rf_orderalias");
    run(&ws, JOIN_SETUP);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT -id AS id FROM a ORDER BY id;",
        &[&["-4"], &["-3"], &["-2"], &["-1"]],
    );
    // `id` is the output built from k, not the base column.
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT k AS id, id AS k FROM a ORDER BY id DESC, k;",
        &[&["2", "3"], &["2", "4"], &["1", "1"], &["1", "2"]],
    );
}

// ── Cross-type equality, casts, qualifiers ──────────────────────────────────

#[test]
fn equality_agrees_across_numeric_types_in_joins_and_set_operations() {
    let ws = Workspace::new("rf_crosstype");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE a (id INT);",
            "INSERT INTO a VALUES (1), (2), (3);",
            "CREATE TABLE b (i BIGINT, f DOUBLE PRECISION, sm SMALLINT);",
            "INSERT INTO b VALUES (1, 1.0, 1), (2, 2.0, 2), (9, 9.5, 9);",
        ],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT a.id, b.i FROM a JOIN b ON a.id = b.f ORDER BY 1;",
        &[&["1", "1"], &["2", "2"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM a LEFT JOIN b ON a.id = b.f;",
        &[&["3"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT id FROM a UNION SELECT i FROM b ORDER BY 1;",
        &[&["1"], &["2"], &["3"], &["9"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT id FROM a INTERSECT SELECT sm FROM b ORDER BY 1;",
        &[&["1"], &["2"]],
    );
}

#[test]
fn temporal_and_rounding_casts() {
    let ws = Workspace::new("rf_casts");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE one (id INT);",
            "INSERT INTO one VALUES (1);",
        ],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT CAST(CAST('2024-01-02 03:04:05' AS TIMESTAMP) AS DATE), \
         CAST(CAST('2024-01-02 03:04:05' AS TIMESTAMP) AS TIME), \
         CAST(1.006 AS NUMERIC(5,2)) FROM one;",
        &[&["2024-01-02", "03:04:05.000000", "1.01"]],
    );
    let out = run(
        &ws,
        &[
            "USE d;",
            "SELECT CAST(9223372036854775808.0 AS BIGINT) FROM one;",
        ],
    );
    assert!(out.contains("out of range for BIGINT"), "{}", out);
}

#[test]
fn unknown_table_qualifier_is_an_error() {
    let ws = Workspace::new("rf_unknownqual");
    run(&ws, JOIN_SETUP);
    let out = run(&ws, &["USE d;", "SELECT zz.id FROM a;"]);
    assert!(out.contains("Column 'zz.id' not found"), "{}", out);
    // A real alias still resolves.
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT t.id FROM a t WHERE t.id = 1;",
        &[&["1"]],
    );
}

// ── Statement-level consistency ─────────────────────────────────────────────

#[test]
fn insert_select_from_the_target_table_reads_a_fixed_snapshot() {
    let ws = Workspace::new("rf_selfinsert");
    let mut setup = vec![
        "CREATE DATABASE d;".to_string(),
        "USE d;".to_string(),
        "CREATE TABLE i (id INT, k INT);".to_string(),
    ];
    setup.extend(numbered_rows("i", 200, 7));
    // Seven doublings: 200 -> 25,600. The last one runs above the cache's
    // 10,000-operation flush interval, where the scan used to pick up rows
    // the same statement had just appended.
    for _ in 0..7 {
        setup.push("INSERT INTO i SELECT id, k FROM i;".to_string());
    }
    run_owned(&ws, &setup);
    expect_rows(&ws, &["USE d;"], "SELECT COUNT(*) FROM i;", &[&["25600"]]);
}

#[test]
fn a_row_the_index_rejects_is_not_inserted() {
    let ws = Workspace::new("rf_idxreject");
    let long_key = "z".repeat(3500);
    let out = run_owned(
        &ws,
        &[
            "CREATE DATABASE d;".to_string(),
            "USE d;".to_string(),
            "CREATE TABLE bk (id INT, k VARCHAR(4000));".to_string(),
            "CREATE INDEX bk_k ON bk (k);".to_string(),
            "INSERT INTO bk VALUES (1, 'short');".to_string(),
            format!("INSERT INTO bk VALUES (2, '{}');", long_key),
        ],
    );
    assert!(out.contains("exceeds the maximum"), "{}", out);
    // The same limit applies to UPDATE, before any row is rewritten.
    let out = run_owned(
        &ws,
        &[
            "USE d;".to_string(),
            format!("UPDATE bk SET k = '{}' WHERE id = 1;", long_key),
        ],
    );
    assert!(
        out.contains("Update failed") && out.contains("exceeds the maximum"),
        "{}",
        out
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT id FROM bk WHERE k = 'short';",
        &[&["1"]],
    );
    // Heap scan and index scan agree: the rejected row is in neither.
    expect_rows(&ws, &["USE d;"], "SELECT id FROM bk;", &[&["1"]]);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT COUNT(*) FROM bk WHERE k >= 'a';",
        &[&["1"]],
    );
}

#[test]
fn cascade_delete_respects_a_restricting_grandchild() {
    let ws = Workspace::new("rf_grandchild");
    let out = run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE p (id INT PRIMARY KEY);",
            "CREATE TABLE c1 (id INT PRIMARY KEY, pid INT, \
                 FOREIGN KEY (pid) REFERENCES p(id) ON DELETE CASCADE);",
            "CREATE TABLE gr (id INT, cid INT REFERENCES c1(id));",
            "INSERT INTO p VALUES (1), (2);",
            "INSERT INTO c1 VALUES (13, 1), (14, 2);",
            "INSERT INTO gr VALUES (40, 13);",
            // Cascading into c1 row 13 would orphan gr row 40.
            "DELETE FROM p WHERE id = 1;",
        ],
    );
    assert!(out.contains("Delete failed"), "{}", out);
    expect_rows(&ws, &["USE d;"], "SELECT COUNT(*) FROM p;", &[&["2"]]);
    expect_rows(&ws, &["USE d;"], "SELECT COUNT(*) FROM c1;", &[&["2"]]);

    // The unrestricted branch still cascades.
    run(&ws, &["USE d;", "DELETE FROM p WHERE id = 2;"]);
    expect_rows(&ws, &["USE d;"], "SELECT id FROM c1;", &[&["13"]]);
}

// ── Names: CTE aliases, column lists, identifier case ───────────────────────

#[test]
fn cte_aliases_and_column_lists_resolve() {
    let ws = Workspace::new("rf_ctealias");
    run(&ws, JOIN_SETUP);
    // A CTE joined to itself: each scan is qualified by its own alias.
    expect_rows(
        &ws,
        &["USE d;"],
        "WITH RECURSIVE c AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM c WHERE n < 3) \
         SELECT x.n, y.n FROM c x JOIN c y ON x.n = y.n ORDER BY 1;",
        &[&["1", "1"], &["2", "2"], &["3", "3"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 3) \
         SELECT n FROM c ORDER BY n;",
        &[&["1"], &["2"], &["3"]],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT s.id FROM (SELECT id FROM a) s WHERE s.id = 2;",
        &[&["2"]],
    );
}

#[test]
fn identifiers_are_case_insensitive() {
    let ws = Workspace::new("rf_case");
    let out = run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE Mixed (Id INT, Val INT);",
            "INSERT INTO mixed VALUES (1, 10);",
            "INSERT INTO MIXED (ID, val) VALUES (2, 20);",
            "UPDATE mixed SET VAL = 99 WHERE id = 2;",
            "DELETE FROM MIXED WHERE ID = 1;",
        ],
    );
    assert!(out.contains("Updated 1 row(s)"), "{}", out);
    assert!(out.contains("Deleted 1 row(s)"), "{}", out);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT id, VAL FROM mixed ORDER BY ID;",
        &[&["2", "99"]],
    );
}

#[test]
fn renaming_a_table_carries_its_foreign_keys() {
    let ws = Workspace::new("rf_renamefk");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE p (id INT PRIMARY KEY);",
            "CREATE TABLE ch (id INT, pid INT REFERENCES p(id));",
            "INSERT INTO p VALUES (1), (2);",
            "INSERT INTO ch VALUES (1, 1);",
            "ALTER TABLE p RENAME TO p2;",
        ],
    );
    // A fresh session: the stored reference follows the rename.
    let out = run(
        &ws,
        &[
            "USE d;",
            "INSERT INTO ch VALUES (2, 2);",
            "INSERT INTO ch VALUES (3, 99);",
            "DELETE FROM p2 WHERE id = 1;",
        ],
    );
    assert!(out.contains("not found in parent table 'p2'"), "{}", out);
    assert!(out.contains("Delete failed"), "{}", out);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT id, pid FROM ch ORDER BY id;",
        &[&["1", "1"], &["2", "2"]],
    );
}

// ── Constant division by zero; aggregate header types ───────────────────────

#[test]
fn constant_division_by_zero_is_reported_as_such() {
    let ws = Workspace::new("rf_div0");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE a (id INT);",
            "INSERT INTO a VALUES (1);",
        ],
    );
    for sql in [
        "SELECT 10 / 0 FROM a;",
        "SELECT 10 % 0 FROM a;",
        "SELECT id / 0 FROM a;",
    ] {
        let out = run(&ws, &["USE d;", sql]);
        assert!(out.contains("Division by zero"), "{}: {}", sql, out);
        assert!(!out.contains("Cannot infer type"), "{}: {}", sql, out);
    }
}

#[test]
fn sum_header_names_the_type_of_the_value_below_it() {
    let ws = Workspace::new("rf_sumtype");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE a (id INT, b BIGINT, r REAL);",
            "INSERT INTO a VALUES (1, 9223372036854775807, 0.1);",
            "INSERT INTO a VALUES (2, 9223372036854775807, 0.1);",
            "CREATE TABLE t (b BIGINT);",
        ],
    );
    let out = run(&ws, &["USE d;", "SELECT SUM(id), SUM(b), SUM(r) FROM a;"]);
    let table = common::parse_last_table(&out);
    assert_eq!(
        table.rows,
        [["3", "18446744073709551614", "0.2"]],
        "{}",
        out
    );
    assert!(out.contains("SUM: BIGINT"), "{}", out);
    assert!(out.contains("SUM: NUMERIC(38,0)"), "{}", out);
    assert!(out.contains("SUM: REAL"), "{}", out);

    // A NUMERIC total assigns to a BIGINT column only when it fits.
    let out = run(
        &ws,
        &[
            "USE d;",
            "INSERT INTO t SELECT SUM(b) FROM a;",
            "INSERT INTO t SELECT SUM(b) FROM a WHERE id = 1;",
        ],
    );
    assert!(out.contains("out of range for BIGINT"), "{}", out);
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT b FROM t;",
        &[&["9223372036854775807"]],
    );
}

// ── D2: a bare column two joined tables share is ambiguous ──────────────────

#[test]
fn ambiguous_bare_column_in_a_join_is_an_error() {
    let ws = Workspace::new("rf_ambiguous");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE l (id INT, k INT);",
            "CREATE TABLE r (id INT, k INT);",
            "INSERT INTO l VALUES (1, 10);",
            "INSERT INTO r VALUES (7, 10);",
        ],
    );
    let out = run(&ws, &["USE d;", "SELECT id FROM l JOIN r ON l.k = r.k;"]);
    assert!(
        out.contains("Column reference 'id' is ambiguous"),
        "{}",
        out
    );
    // Qualified names and USING's merged column still work.
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT l.id, r.id, k FROM l JOIN r USING (k);",
        &[&["1", "7", "10"]],
    );
}

// ── D10: quoted identifiers keep their spelling ─────────────────────────────

#[test]
fn quoted_column_names_work_end_to_end() {
    let ws = Workspace::new("rf_quoted");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE Mixed (\"my col\" INT, \"Total\" INT);",
            "INSERT INTO Mixed VALUES (3, 30);",
            "INSERT INTO Mixed (\"Total\", \"my col\") VALUES (10, 1);",
        ],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT \"my col\" FROM Mixed WHERE \"Total\" > 5 ORDER BY \"my col\";",
        &[&["1"], &["3"]],
    );
    run(
        &ws,
        &[
            "USE d;",
            "UPDATE Mixed SET \"my col\" = 9 WHERE \"Total\" = 10;",
        ],
    );
    expect_rows(
        &ws,
        &["USE d;"],
        "SELECT \"my col\" FROM Mixed ORDER BY 1;",
        &[&["3"], &["9"]],
    );
}

#[test]
fn quoted_table_names_work_end_to_end() {
    let ws = Workspace::new("rf_quoted_table");
    run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE TABLE \"My Table\" (id INT);",
            "INSERT INTO \"My Table\" VALUES (1);",
        ],
    );
    // A second process sees the table: it was stored under its real name.
    expect_rows(&ws, &["USE d;"], "SELECT * FROM \"My Table\";", &[&["1"]]);
    let out = run(&ws, &["USE d;", "CREATE TABLE \"a/b\" (id INT);"]);
    assert!(out.contains("Invalid table name"), "{}", out);
}

#[test]
fn quoted_database_index_and_view_names_work_end_to_end() {
    let ws = Workspace::new("rf_quoted_objects");
    run(
        &ws,
        &[
            "CREATE DATABASE \"Sales Db\";",
            "USE \"Sales Db\";",
            "CREATE TABLE \"My Table\" (\"my col\" INT, \"Total\" INT);",
            "INSERT INTO \"My Table\" VALUES (1, 10), (2, 20);",
            "CREATE INDEX \"My Idx\" ON \"My Table\" (\"my col\");",
            "CREATE VIEW \"My View\" AS SELECT \"my col\" FROM \"My Table\" WHERE \"Total\" > 10;",
        ],
    );
    // The database directory has its real name, with no quote characters.
    let shown = run(&ws, &["SHOW DATABASES;"]);
    assert!(shown.contains("Sales Db"), "{}", shown);
    assert!(!shown.contains("\"Sales Db\""), "{}", shown);
    // A second process can connect, read through the view and the index.
    expect_rows(
        &ws,
        &["USE \"Sales Db\";"],
        "SELECT * FROM \"My View\";",
        &[&["2"]],
    );
    expect_rows(
        &ws,
        &["USE \"Sales Db\";"],
        "SELECT \"Total\" FROM \"My Table\" WHERE \"my col\" = 1;",
        &[&["10"]],
    );
    // Dropping works by quoted name.
    run(
        &ws,
        &[
            "USE \"Sales Db\";",
            "DROP VIEW \"My View\";",
            "DROP INDEX \"My Idx\";",
        ],
    );
    let out = run(&ws, &["USE \"Sales Db\";", "SELECT * FROM \"My View\";"]);
    assert!(
        out.to_lowercase().contains("error") || out.to_lowercase().contains("not"),
        "{}",
        out
    );
    // The dropped index can be created again.
    let out = run(
        &ws,
        &[
            "USE \"Sales Db\";",
            "CREATE INDEX \"My Idx\" ON \"My Table\" (\"Total\");",
        ],
    );
    assert!(!out.to_lowercase().contains("exist"), "{}", out);
    run(&ws, &["DROP DATABASE \"Sales Db\";"]);
    let shown = run(&ws, &["SHOW DATABASES;"]);
    assert!(!shown.contains("Sales Db"), "{}", shown);

    // Names a file cannot hold are rejected.
    let out = run(&ws, &["CREATE DATABASE \"a/b\";"]);
    assert!(out.contains("Invalid database name"), "{}", out);
    let out = run(
        &ws,
        &[
            "CREATE DATABASE d;",
            "USE d;",
            "CREATE INDEX \"a/b\" ON t (c);",
        ],
    );
    assert!(out.contains("Invalid index name"), "{}", out);
    let out = run(&ws, &["USE d;", "CREATE VIEW \"a/b\" AS SELECT 1;"]);
    assert!(out.contains("Invalid view name"), "{}", out);
}
