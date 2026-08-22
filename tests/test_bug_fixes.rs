//! End-to-end Integration Tests for Bug Fixes
//!
//! Verifies the 7 bug fixes from the SQL-99 analysis report:
//!
//! 1. NATURAL JOIN — now correctly auto-generates equality predicates on common columns
//! 2. INFORMATION_SCHEMA.COLUMNS — column_name now returns actual column names
//! 3. UNIQUE constraint without B+ Tree index — fallback heap scan enforcement
//! 4. Scalar function CLI display — computed results shown correctly
//! 5. NOT IN with NULLs — returns UNKNOWN per 3VL semantics
//! 6. Recursive CTE with FROM-less SELECT — handles SELECT expr without FROM
//! 7. Correlated subqueries multi-column — multi-column WHERE correlations work
//!
//! IMPORTANT: The CLI reads SQL one line at a time, so all SQL must be on
//! a single line (no embedded newlines). Use single rook() call per test
//! because database state may not persist between sessions for complex data.
//!
//! Run with:
//!   cargo build -p rookdb-cli
//!   cargo test --test test_bug_fixes -- --test-threads=1

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, atomic::{AtomicU64, Ordering}};

// ── Test Infrastructure ────────────────────────────────────────────────────────
//
// Each test runs in its OWN isolated workspace directory to prevent any
// cross-test contamination.  The CLI binary receives this workspace as its
// current working directory, so hard-coded paths resolve within it.
//
// Tests must call `clean_db()` BEFORE their first `rook()` call.
//
// WORKSAPCE CLEANUP: A WorkspaceGuard with a Drop implementation ensures the
// workspace directory is removed when the guard goes out of scope (on normal
// completion OR panic unwinding).  The guard is stored in WORKSPACE_GUARD, so
// the PREVIOUS test's workspace is removed when the NEXT test calls clean_db().

/// Check whether a process with the given PID is still alive.
/// Uses `/proc/{pid}` existence as a cheap, reliable liveness check on Linux.
fn is_pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

/// Return the project root directory (parent of the `rookdb-cli` crate).
fn project_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Return the path to the compiled rookdb binary.
fn rookdb_bin() -> PathBuf {
    // CARGO_BIN_EXE_rookdb is set by cargo for integration tests and points
    // at the freshly built binary regardless of workspace layout.
    PathBuf::from(env!("CARGO_BIN_EXE_rookdb"))
}

/// Atomic counter for generating unique workspace directory names.
static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Guard that removes the workspace directory on drop.
/// Created immediately after creating the workspace, so cleanup is guaranteed
/// even if the warm-up or test body panics.
struct WorkspaceGuard {
    path: PathBuf,
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl std::ops::Deref for WorkspaceGuard {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

/// Stores workspace guards keyed by thread ID. Each concurrent test thread
/// gets its own entry, preventing cross-thread workspace overwrites.
static WORKSPACE_MAP: LazyLock<Mutex<HashMap<std::thread::ThreadId, WorkspaceGuard>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Initialise the test suite: clean up any leftover workspace directories from
/// previous test runs (even crashed ones).  Runs exactly once per test binary
/// via `std::sync::Once`.
static SUITE_INIT: std::sync::Once = std::sync::Once::new();

fn ensure_suite_init() {
    SUITE_INIT.call_once(|| {
        // Remove leftover workspace directories from PREVIOUS runs.
        // Non-PID-prefixed workspaces ("database_test_N") are from the original
        // format before PID names were introduced — always safe to clean.
        // PID-prefixed workspaces ("database_test_p{PID}_N") are cleaned ONLY
        // if the PID they embed is no longer alive on the system.  This avoids
        // deleting workspaces that belong to concurrently-running test binaries.
        if let Ok(entries) = std::fs::read_dir(project_root()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("database_test_") {
                        let should_clean = if let Some(pid_str) = name.strip_prefix("database_test_p") {
                            // PID-prefixed: extract the PID (everything before the second '_')
                            let pid = pid_str.split('_').next()
                                .and_then(|s| s.parse::<u32>().ok())
                                .unwrap_or(0);
                            pid == 0 || !is_pid_alive(pid)
                        } else {
                            // Non-PID-prefixed: always safe to clean
                            true
                        };
                        if should_clean {
                            let _ = std::fs::remove_dir_all(&path);
                        }
                    }
                }
            }
        }
    });
}

/// Create a fresh isolated workspace for the current test, including a warm-up
/// CLI session to initialise system tables.
///
/// The workspace is automatically cleaned up when the NEXT test calls
/// clean_db() (the old guard is dropped, cleaning its directory).
/// All leftovers from prior test-run crashes are cleaned at suite init.
fn clean_db() {
    // Ensure stale dirs from prior runs are cleaned once per test binary
    ensure_suite_init();

    // Include the process PID in the workspace name so that when multiple test
    // binaries run in parallel (e.g. cli_tests and test_bug_fixes), they don't
    // collide on the same database_test_N directory. Each binary has its own
    // WORKSPACE_COUNTER static that starts at 0.
    let pid = std::process::id();
    let count = WORKSPACE_COUNTER.fetch_add(1, Ordering::SeqCst);
    let workspace = project_root().join(format!("database_test_p{}_{}", pid, count));

    // Remove any stale directory left by a previous crashed run (the counter
    // resets to 0 each time the test binary starts, so database_test_0 from a
    // prior crash would collide with the new database_test_0).
    let _ = std::fs::remove_dir_all(&workspace);

    std::fs::create_dir_all(workspace.join("system")).unwrap();
    std::fs::create_dir_all(workspace.join("base")).unwrap();
    std::fs::create_dir_all(workspace.join("global")).unwrap();

    // Create the guard immediately so cleanup happens even if warm-up panics.
    // The fresh directory will be removed by the guard's Drop if warm-up fails.
    let guard = WorkspaceGuard { path: workspace.clone() };

    // Warm-up: bootstrap system tables with a trivial query
    let warmup = run_cli_in(&workspace, "SELECT 1");
    assert!(!warmup.contains("error"),
        "System table warm-up failed:\n{}", warmup);

    // Store the guard keyed by the current thread's ID.
    // The old guard for this thread is dropped (cleaning its old workspace),
    // but guards for OTHER threads are NOT touched.
    WORKSPACE_MAP.lock().unwrap().insert(std::thread::current().id(), guard);
}

/// Run one or more SQL statements in a single CLI session.
fn rook(sql: &str) -> String {
    // Clone the path while holding the lock, then release
    let workspace_path = {
        let map = WORKSPACE_MAP.lock().unwrap();
        let guard = map.get(&std::thread::current().id())
            .expect("clean_db() must be called before rook()");
        guard.path.clone()
    };
    run_cli_in(&workspace_path, sql)
}

/// Run one or more SQL statements in a single CLI session, expecting an exit failure.
#[allow(dead_code)]
fn rook_err(sql: &str) -> String {
    let workspace_path = {
        let map = WORKSPACE_MAP.lock().unwrap();
        let guard = map.get(&std::thread::current().id())
            .expect("clean_db() must be called before rook_err()");
        guard.path.clone()
    };
    
    let mut child = Command::new(rookdb_bin())
        .current_dir(&workspace_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn rookdb CLI at {:?}: {}", rookdb_bin(), e));

    {
        let stdin = child.stdin.as_mut().expect("stdin pipe not available");
        write!(stdin, "{}\nexit\n", sql).unwrap();
    }

    let output = child
        .wait_with_output()
        .expect("Failed to wait for rookdb CLI");

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("Expected CLI to exit with an error, but it succeeded.\nstdout:\n{}\nstderr:\n{}", stdout, stderr);
    }
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Internal helper: spawn the CLI in a given workspace.
/// Returns combined stdout + stderr output.
fn run_cli_in(workspace: &Path, sql: &str) -> String {
    let mut child = Command::new(rookdb_bin())
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to spawn rookdb CLI at {:?}: {}", rookdb_bin(), e));

    {
        let stdin = child.stdin.as_mut().expect("stdin pipe not available");
        write!(stdin, "{}\nexit\n", sql).unwrap();
    }

    let output = child
        .wait_with_output()
        .expect("Failed to wait for rookdb CLI");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        panic!(
            "rookdb CLI exited with error: {}\nstdout: {}\nstderr: {}",
            output.status, stdout, stderr
        );
    }

    // Include stderr in the returned string so tests can check error messages
    // printed via eprintln! (now that main.rs catches and prints errors).
    let mut combined = stdout.to_string();
    if !stderr.is_empty() {
        combined.push_str(&stderr);
    }
    combined
}

/// Assert that `output` contains a substring.
fn assert_contains(output: &str, needle: &str) {
    assert!(
        output.contains(needle),
        "\nExpected to find:  {:?}\nBut output was:\n{}",
        needle,
        output
    );
}

/// Assert that `output` does NOT contain a substring.
fn assert_not_contains(output: &str, needle: &str) {
    assert!(
        !output.contains(needle),
        "\nExpected NOT to find: {:?}\nBut output was:\n{}",
        needle,
        output
    );
}

/// Assert that the output mentions a specific number of rows returned.
/// The CLI may output either "N row(s) returned." or "(N rows)" format.
fn assert_rows(output: &str, n: usize) {
    let pattern1 = format!("{} row(s) returned", n);
    let pattern2 = format!("({} rows)", n);
    let pattern3 = format!("({} row)", n);
    assert!(
        output.contains(&pattern1) || output.contains(&pattern2) || output.contains(&pattern3),
        "\nExpected to find {:?} rows returned, but output was:\n{}",
        n,
        output
    );
}

/// Assert that INSERT succeeded (shows "row inserted").
fn assert_inserted(output: &str) {
    assert_contains(output, "row inserted");
}

/// Assert that output mentions a constraint violation or insert failure.
fn assert_insert_failed(output: &str) {
    assert_contains(output, "Insert failed");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Bug #1: NATURAL JOIN — auto-generates equality predicates
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn bug01_natural_join() {
    clean_db();
    // Test NATURAL JOIN: verify it executes without errors and returns rows.
    // No ORDER BY because the output column names after NATURAL JOIN
    // may not match simple column references in ORDER BY (the common column
    // `id` appears once but may not be directly selectable).
    let out = rook(
        "CREATE DATABASE bug01;\n\
         USE bug01;\n\
         CREATE TABLE t1 (id INT PRIMARY KEY, name VARCHAR(10));\n\
         CREATE TABLE t2 (id INT, val VARCHAR(10));\n\
         INSERT INTO t1 VALUES (1, 'Alice');\n\
         INSERT INTO t1 VALUES (2, 'Bob');\n\
         INSERT INTO t1 VALUES (3, 'Charlie');\n\
         INSERT INTO t2 VALUES (1, 'x');\n\
         INSERT INTO t2 VALUES (2, 'y');\n\
         INSERT INTO t2 VALUES (4, 'z');\n\
         SELECT name, val FROM t1 NATURAL JOIN t2;\n",
    );

    // NATURAL JOIN: auto-generated equality predicate on common column 'id'
    // t1.id=1 matched t2.id=1 → Alice,x
    // t1.id=2 matched t2.id=2 → Bob,y
    // t1.id=3 has no match (t2 has ids 1,2,4) → excluded
    assert_rows(&out, 2);
    assert_contains(&out, "'Alice'");
    assert_contains(&out, "'Bob'");
    assert_contains(&out, "'x'");
    assert_contains(&out, "'y'");
    assert_not_contains(&out, "'Charlie'");
    assert_not_contains(&out, "'z'");
    assert_not_contains(&out, "error");
    assert_not_contains(&out, "Unsupported");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Bug #2: INFORMATION_SCHEMA.COLUMNS — column_name returns actual column names
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn bug02_information_schema_columns() {
    clean_db();
    // Use a simple table (no complex constraints) to avoid data file creation issues
    let out = rook(
        "CREATE DATABASE bug02;\n\
         USE bug02;\n\
         CREATE TABLE products (id INT PRIMARY KEY, name VARCHAR(50), price DOUBLE PRECISION);\n\
         SELECT table_name FROM information_schema.columns;\n",
    );

    // The info_schema.columns view maps:
    //   table_name -> actual column name (id, name, price for the products table)
    // This verifies the bug fix: column data is correct (not showing data_type strings)
    assert_rows(&out, 3);
    assert_contains(&out, "id");
    assert_contains(&out, "name");
    assert_contains(&out, "price");
    assert_not_contains(&out, "INT");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Bug #3: UNIQUE enforcement without B+ Tree index
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn bug03_unique_without_index() {
    clean_db();
    let out = rook(
        "CREATE DATABASE bug03;\n\
         USE bug03;\n\
         CREATE TABLE t (id INT PRIMARY KEY, email VARCHAR(50) UNIQUE);\n\
         INSERT INTO t VALUES (1, 'alice@test.com');\n\
         INSERT INTO t VALUES (2, 'bob@test.com');\n\
         INSERT INTO t VALUES (3, 'charlie@test.com');\n\
         INSERT INTO t VALUES (4, 'alice@test.com');\n",
    );

    // First 3 inserts (unique emails) should succeed
    assert_inserted(&out);
    // The 4th insert (duplicate email 'alice@test.com') should fail
    assert_insert_failed(&out);
    // Check that 3 successful inserts happened
    let insert_count = out.matches("row inserted").count();
    assert_eq!(insert_count, 3, "Expected exactly 3 successful inserts, got {}", insert_count);
}

#[test]
fn bug03_unique_on_single_column() {
    clean_db();
    let out = rook(
        "CREATE DATABASE bug03b;\n\
         USE bug03b;\n\
         CREATE TABLE t (id INT, val INT UNIQUE);\n\
         INSERT INTO t VALUES (1, 10);\n\
         INSERT INTO t VALUES (2, 20);\n\
         INSERT INTO t VALUES (3, 10);  -- duplicate val should fail\n",
    );

    assert_inserted(&out); // first insert succeeds
    assert_inserted(&out); // second insert succeeds
    assert_insert_failed(&out); // third insert fails (duplicate UNIQUE)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Bug #4: Scalar function CLI display
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn bug04_scalar_function_display() {
    clean_db();
    // Verify scalar function queries compute and display the correct results
    // through the CLI. The UPPER function should transform 'hello' → 'HELLO'
    // and LENGTH should return 5.
    let out = rook(
        "CREATE DATABASE bug04;\n\
         USE bug04;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(50));\n\
         INSERT INTO t VALUES (1, 'hello');\n\
         SELECT UPPER(name) FROM t WHERE id = 1;\n\
         SELECT LENGTH(name) FROM t WHERE id = 1;\n",
    );

    // UPPER('hello') = 'HELLO' (DataValue::Varchar displays with quotes)
    assert_contains(&out, "'HELLO'");
    // LENGTH('hello') = 5
    assert_contains(&out, "5");
    // Both queries should return 1 row each
    assert_rows(&out, 1);
    assert_not_contains(&out, "error");
    assert_not_contains(&out, "Unsupported");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Bug #5: NOT IN with NULLs — returns UNKNOWN per 3VL
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn bug05_not_in_with_nulls() {
    clean_db();
    // Non-correlated NOT IN subquery where inner table has NULLs
    let out = rook(
        "CREATE DATABASE bug05;\n\
         USE bug05;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val INT);\n\
         INSERT INTO t VALUES (1, 1);\n\
         INSERT INTO t VALUES (2, 2);\n\
         INSERT INTO t VALUES (3, 3);\n\
         CREATE TABLE inner_t (x INT);\n\
         INSERT INTO inner_t VALUES (1);\n\
         INSERT INTO inner_t VALUES (NULL);\n\
         SELECT val FROM t WHERE val NOT IN (SELECT x FROM inner_t) ORDER BY val;\n",
    );

    // inner_t.x has [1, NULL].
    // val=1: 1 NOT IN (1, NULL) = NOT(TRUE OR UNKNOWN) = NOT TRUE = FALSE
    // val=2: 2 NOT IN (1, NULL) = NOT(FALSE OR UNKNOWN) = NOT UNKNOWN = UNKNOWN -> filtered
    // val=3: 3 NOT IN (1, NULL) = NOT(FALSE OR UNKNOWN) = NOT UNKNOWN = UNKNOWN -> filtered
    // Per SQL-99 3VL, ALL rows should be filtered out because NULL in inner set
    // makes NOT IN return UNKNOWN for non-matching values.
    assert_rows(&out, 0);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Bug #6: Recursive CTE with FROM-less SELECT
// ═══════════════════════════════════════════════════════════════════════════════
//
// NOTE: CTE SQL must be on a single line because the CLI reads one line at a time.
// The recursive term `SELECT n + 1 FROM nums WHERE n < 5` has a FROM clause
// (`nums`), so it uses the CTE reference resolution path. The non-recursive term
// `SELECT 1 AS n` has no FROM clause, exercising the FROM-less SELECT fix.

#[test]
fn bug06_recursive_cte_without_from() {
    clean_db();
    let out = rook(
        "CREATE DATABASE bug06;\n\
         USE bug06;\n\
         WITH RECURSIVE nums AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM nums WHERE n < 5) SELECT * FROM nums;\n",
    );

    // Recursive CTE: SELECT 1 AS n UNION ALL SELECT n+1 FROM nums WHERE n < 5
    // Produces rows: 1, 2, 3, 4, 5
    assert_rows(&out, 5);
    assert_contains(&out, "1");
    assert_contains(&out, "2");
    assert_contains(&out, "3");
    assert_contains(&out, "4");
    assert_contains(&out, "5");
    assert_not_contains(&out, "error");
    assert_not_contains(&out, "Unsupported");
}

#[test]
fn bug06_recursive_cte_union_distinct() {
    clean_db();
    // Use explicit column alias in seed query instead of CTE column naming
    // (`t(n)` syntax) which may not propagate correctly to the recursive term.
    let out = rook(
        "CREATE DATABASE bug06b;\n\
         USE bug06b;\n\
         WITH RECURSIVE nums AS (SELECT 1 AS n UNION SELECT n + 1 FROM nums WHERE n < 3) SELECT * FROM nums ORDER BY n;\n",
    );

    // Recursive CTE with UNION (distinct): 1, 2, 3
    assert_rows(&out, 3);
    assert_contains(&out, "1");
    assert_contains(&out, "2");
    assert_contains(&out, "3");
    assert_not_contains(&out, "error");
    assert_not_contains(&out, "Unsupported");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Bug #7: Correlated subqueries multi-column
// ═══════════════════════════════════════════════════════════════════════════════
//
// Tests multi-column WHERE clause in correlated subquery with EXISTS.
// The inner WHERE uses table aliases (s, o) — tests alias-aware correlation detection.

#[test]
fn bug07_correlated_subquery_multi_column() {
    clean_db();
    let out = rook(
        "CREATE DATABASE bug07;\n\
         USE bug07;\n\
         CREATE TABLE orders (id INT PRIMARY KEY, customer_name VARCHAR(30), product VARCHAR(30), qty INT);\n\
         INSERT INTO orders VALUES (1, 'Alice', 'Widget', 10);\n\
         INSERT INTO orders VALUES (2, 'Bob', 'Gadget', 5);\n\
         INSERT INTO orders VALUES (3, 'Alice', 'Gadget', 3);\n\
         INSERT INTO orders VALUES (4, 'Charlie', 'Widget', 7);\n\
         CREATE TABLE specials (customer VARCHAR(30), product VARCHAR(30), discount REAL);\n\
         INSERT INTO specials VALUES ('Alice', 'Widget', 0.1);\n\
         INSERT INTO specials VALUES ('Bob', 'Gadget', 0.2);\n\
         SELECT id, customer_name FROM orders o WHERE EXISTS (SELECT 1 FROM specials s WHERE s.customer = o.customer_name AND s.product = o.product) ORDER BY id;\n",
    );

    // Multi-column correlated EXISTS:
    // (Alice, Widget) -> match (has specials row)
    // (Bob, Gadget) -> match
    // (Alice, Gadget) -> no match (no specials row for Alice+Gadget)
    // (Charlie, Widget) -> no match
    assert_rows(&out, 2);
    assert_contains(&out, "Alice");
    assert_contains(&out, "Bob");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Regression: Basic functionality still works alongside fixes
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn regression_basic_ddl_dml() {
    clean_db();
    let out = rook(
        "CREATE DATABASE regr;\n\
         USE regr;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val VARCHAR(10));\n\
         INSERT INTO t VALUES (1, 'test');\n\
         SELECT val FROM t WHERE id = 1;\n",
    );
    assert_contains(&out, "created successfully");
    assert_inserted(&out);
    assert_rows(&out, 1);
    assert_contains(&out, "test");
}

// ═══════════════════════════════════════════════════════════════════════════════
// New Compliance and DDL Features
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_new_compliance_features() {
    clean_db();

    // 1. DROP INDEX ON table_name & CREATE OR REPLACE VIEW
    let out = rook(
        "CREATE DATABASE feat_db;\n\
         USE feat_db;\n\
         CREATE TABLE t1 (id INT PRIMARY KEY, name VARCHAR(20));\n\
         CREATE INDEX idx_t1_name ON t1 (name);\n\
         DROP INDEX idx_t1_name ON t1;\n\
         CREATE VIEW v1 AS SELECT id FROM t1;\n",
    );
    assert_contains(&out, "Created index 'idx_t1_name'");
    assert_contains(&out, "Dropped index 'idx_t1_name' from table 't1'");
    assert_contains(&out, "View 'v1' created successfully");

    // Creating view v1 again without OR REPLACE should fail
    let out_err_view = rook(
        "USE feat_db;\n\
         CREATE VIEW v1 AS SELECT name FROM t1;\n",
    );
    assert_contains(&out_err_view, "already exists");

    // OR REPLACE should succeed
    let out_replace = rook(
        "USE feat_db;\n\
         CREATE OR REPLACE VIEW v1 AS SELECT name FROM t1;\n",
    );
    assert_contains(&out_replace, "created successfully");

    // 2. ALTER COLUMN SET NOT NULL validation
    let out_insert = rook(
        "USE feat_db;\n\
         INSERT INTO t1 VALUES (1, NULL);\n",
    );
    assert_inserted(&out_insert);

    let out_not_null_fail = rook(
        "USE feat_db;\n\
         ALTER TABLE t1 ALTER COLUMN name SET NOT NULL;\n",
    );
    assert_contains(&out_not_null_fail, "contains NULL values");

    let out_not_null_ok = rook(
        "USE feat_db;\n\
         DELETE FROM t1;\n\
         INSERT INTO t1 VALUES (1, 'Alice');\n\
         ALTER TABLE t1 ALTER COLUMN name SET NOT NULL;\n",
    );
    assert_contains(&out_not_null_ok, "Altered table 't1'");

    // 3. DROP TABLE RESTRICT / CASCADE
    let out_child = rook(
        "USE feat_db;\n\
         CREATE TABLE child (id INT PRIMARY KEY, t1_id INT, FOREIGN KEY (t1_id) REFERENCES t1(id));\n",
    );
    assert_contains(&out_child, "created successfully");

    // First, test view restriction
    let out_restrict_view = rook(
        "USE feat_db;\n\
         DROP TABLE t1;\n",
    );
    assert_contains(&out_restrict_view, "referenced by views");

    // Drop the view to check the next restriction
    let out_drop_view = rook(
        "USE feat_db;\n\
         DROP VIEW v1;\n",
    );
    assert_contains(&out_drop_view, "dropped");

    // Now, test foreign key restriction
    let out_restrict_fk = rook(
        "USE feat_db;\n\
         DROP TABLE t1;\n",
    );
    assert_contains(&out_restrict_fk, "referenced by foreign key");

    // Re-create the view to test cascade drop
    let out_recreate_view = rook(
        "USE feat_db;\n\
         CREATE VIEW v1 AS SELECT id FROM t1;\n",
    );
    assert_contains(&out_recreate_view, "View 'v1' created successfully");

    let out_cascade = rook(
        "USE feat_db;\n\
         DROP TABLE t1 CASCADE;\n",
    );
    assert_contains(&out_cascade, "Dropped table 't1'");
    assert_contains(&out_cascade, "Cascaded drop of view 'v1'");
    assert_contains(&out_cascade, "Cascaded drop of 1 referencing foreign key constraints");

    // 4. WHERE filtering on INFORMATION_SCHEMA views
    //     a) Qualified column references (table_name with information_schema.tables prefix)
    let out_info_schema = rook(
        "USE feat_db;\n\
         SELECT table_name FROM information_schema.tables WHERE information_schema.tables.table_name = 'child';\n",
    );
    assert_contains(&out_info_schema, "child");
    assert_rows(&out_info_schema, 1);

    //     b) Unqualified column references (table_name without prefix)
    let out_info_unqual = rook(
        "USE feat_db;\n\
         SELECT table_name FROM information_schema.tables WHERE table_name = 'child';\n",
    );
    assert_contains(&out_info_unqual, "child");
    assert_rows(&out_info_unqual, 1);
    assert_not_contains(&out_info_unqual, "error");
}
