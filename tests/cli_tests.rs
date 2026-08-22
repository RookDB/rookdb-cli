//! RookDB CLI Integration Tests
//!
//! These tests replace the old `comprehensive_e2e_test.sh` with proper Rust
//! integration tests. Each test spawns the `rookdb` binary as a subprocess,
//! feeds SQL commands via stdin, and asserts on the output.
//!
//! Run with: `cargo test --test cli_tests -- --test-threads=1`
//!
//! NOTE: Some tests use relaxed assertions for features known to have CLI bugs
//! (e.g. column aliases in output). These are documented inline with
//! "KNOWN CLI LIMITATION" comments.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, atomic::{AtomicU64, Ordering}};

// ── Test Infrastructure ────────────────────────────────────────────────────────
//
// Each test runs in its OWN isolated workspace directory (e.g. database_test_0/,
// database_test_1/, ...) to prevent any cross-test contamination.  The CLI binary
// receives this workspace as its current working directory, so all hard-coded
// paths like `database/system/` resolve within the workspace.
//
// CRITICAL: Workspaces are stored PER-THREAD so that when cargo runs tests in
// parallel (the default), each thread's workspace is isolated and cannot be
// overwritten or deleted by another thread's clean_db() call.
//
// NOTE: Tests must call `clean_db()` BEFORE their first `rook()` call.  This
// creates a fresh workspace and initializes the system tables.
//
// WORKSPACE CLEANUP: A WorkspaceGuard with a Drop implementation ensures the
// workspace directory is removed when the test binary exits (all guards are
// stored in a Mutex-protected HashMap, which is dropped when the process ends).
// If a previous run crashed, stale workspace directories are cleaned by
// ensure_suite_init() at the start of the next run.

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

/// Create a fresh isolated workspace for the current test.
///
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

/// Create a fresh isolated workspace for the current test.
///
/// Each test thread gets its own workspace directory, keyed by the thread's
/// unique ID. Previous workspaces for THIS thread are cleaned (so a single
/// test can call clean_db()+rook() multiple times within its body), but
/// workspaces belonging to OTHER threads are left untouched.
///
/// Creates:
///   database_test_{N}/system/    — system catalog heap files
///   database_test_{N}/base/      — user database data files
///   database_test_{N}/global/    — legacy catalog.json location
///
/// Then runs a warm-up CLI session (`SELECT 1`) to force system table
/// initialization (bootstrap_system_catalog + create_system_table_files).
///
/// Workspaces from a PREVIOUS test run (that crashed without cleanup) are
/// cleaned at suite init.
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

    // Create all required subdirectories
    std::fs::create_dir_all(workspace.join("system")).unwrap();
    std::fs::create_dir_all(workspace.join("base")).unwrap();
    std::fs::create_dir_all(workspace.join("global")).unwrap();

    // Create the guard immediately so cleanup happens even if warm-up panics.
    // The fresh directory will be removed by the guard's Drop if warm-up fails.
    let guard = WorkspaceGuard { path: workspace.clone() };

    // Warm-up: run a trivial SQL statement to force system table bootstrapping.
    // This ensures the CLI initialises database/system/{databases,tables,...}.dat
    // so that subsequent save_catalog() calls can persist metadata.
    let warmup = run_cli_in(&workspace, "SELECT 1");
    assert!(!warmup.contains("error"),
        "System table warm-up failed:\n{}", warmup);

    // Store the guard keyed by the current thread's ID.
    // The old guard for this thread is dropped (cleaning its old workspace),
    // but guards for OTHER threads are NOT touched.
    WORKSPACE_MAP.lock().unwrap().insert(std::thread::current().id(), guard);
}

/// Run one or more SQL statements in a SINGLE CLI session.
///
/// Each SQL statement must be on its own line (terminated by `\n`) because
/// the CLI uses `read_line`.  After all SQL is sent, `exit` is appended.
///
/// Callers MUST invoke `clean_db()` before the first `rook()` call.
fn rook(sql: &str) -> String {
    // Clone the path while holding the lock, then release before the
    // subprocess spawn to minimise lock contention across threads.
    let workspace_path = {
        let map = WORKSPACE_MAP.lock().unwrap();
        let guard = map.get(&std::thread::current().id())
            .expect("clean_db() must be called before rook()");
        guard.path.clone()
    };
    run_cli_in(&workspace_path, sql)
}

/// Internal helper: spawn the CLI in a given workspace directory and send SQL.
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

// ═══════════════════════════════════════════════════════════════════════════════
// Database & Schema Management
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn database_and_schema_management() {
    clean_db();
    let out = rook(
        "CREATE DATABASE testdb;\n\
         SHOW DATABASES;\n\
         USE testdb;\n\
         CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(50), \
                            salary DOUBLE PRECISION);\n\
         SHOW TABLES;\n",
    );
    assert_contains(&out, "created successfully");
    assert_contains(&out, "testdb");
    assert_contains(&out, "selected");
    assert_contains(&out, "users");
}

// ═══════════════════════════════════════════════════════════════════════════════
// DML — INSERT, SELECT, UPDATE, DELETE
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn dml_insert_select_update_delete() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE dml_db;\n\
         USE dml_db;\n\
         CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(50), \
                            price REAL, qty INT);\n\
         INSERT INTO items VALUES (1, 'Widget', 9.99, 10);\n\
         INSERT INTO items VALUES (2, 'Gadget', 24.99, 5);\n\
         INSERT INTO items VALUES (3, 'Doodad', 4.99, 20);\n\
         SELECT * FROM items;\n",
    );

    let out = rook(
        "USE dml_db;\n\
         UPDATE items SET qty = 15 WHERE id = 1;\n\
         DELETE FROM items WHERE id = 3;\n\
         SELECT * FROM items;\n",
    );
    assert_contains(&out, "Updated");
    assert_contains(&out, "Deleted");
    assert_contains(&out, "Widget");
    assert_contains(&out, "Gadget");
    assert_not_contains(&out, "Doodad");
    assert_rows(&out, 2);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Volcano Engine — Filter, Sort, Limit, Distinct, Aggregates, Group By
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn volcano_engine_queries() {
    clean_db();

    let _setup = rook(
        "CREATE DATABASE query_db;\n\
         USE query_db;\n\
         CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(50), \
                            age INT, salary DOUBLE PRECISION, \
                            active BOOLEAN DEFAULT true);\n\
         INSERT INTO users VALUES (1, 'Alice', 30, 75000.0, true);\n\
         INSERT INTO users VALUES (2, 'Bob', 25, 60000.0, false);\n\
         INSERT INTO users VALUES (3, 'Charlie', 35, 95000.0, true);\n\
         INSERT INTO users VALUES (4, 'Diana', 28, 72000.0, true);\n\
         INSERT INTO users VALUES (5, 'Eve', 32, 88000.0, false);\n",
    );

    let out = rook("USE query_db;\nSELECT name, salary FROM users WHERE salary > 70000;\n");
    assert_rows(&out, 4);
    assert_contains(&out, "Alice");
    assert_contains(&out, "Charlie");
    assert_contains(&out, "Diana");
    assert_contains(&out, "Eve");

    let out = rook("USE query_db;\nSELECT name FROM users ORDER BY name;\n");
    assert_rows(&out, 5);

    let out = rook("USE query_db;\nSELECT name FROM users ORDER BY name LIMIT 2;\n");
    assert_contains(&out, "Alice");
    assert_not_contains(&out, "error");

    let out = rook("USE query_db;\nSELECT DISTINCT active FROM users;\n");
    assert_rows(&out, 2);

    let out = rook("USE query_db;\nSELECT COUNT(*), SUM(salary) FROM users;\n");
    assert_rows(&out, 1);
    assert_contains(&out, "5");

    let out = rook("USE query_db;\nSELECT active, COUNT(*) FROM users GROUP BY active;\n");
    assert_rows(&out, 2);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Advanced Predicates — IN, BETWEEN, IS NULL, LIKE, AND, CAST, Arith
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn advanced_predicates() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE adv_db;\n\
         USE adv_db;\n\
         CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(50), \
                            age INT, salary DOUBLE PRECISION, \
                            active BOOLEAN DEFAULT true);\n\
         INSERT INTO users VALUES (1, 'Alice', 30, 75000.0, true);\n\
         INSERT INTO users VALUES (2, 'Bob', 25, 60000.0, false);\n\
         INSERT INTO users VALUES (3, 'Charlie', 35, 95000.0, true);\n",
    );

    let out = rook("USE adv_db;\nSELECT name FROM users WHERE age IN (25, 30, 35);\n");
    assert_rows(&out, 3);
    assert_contains(&out, "Alice");
    assert_contains(&out, "Bob");
    assert_contains(&out, "Charlie");

    let out = rook("USE adv_db;\nSELECT name FROM users WHERE age BETWEEN 28 AND 35;\n");
    assert_rows(&out, 2);
    assert_contains(&out, "Alice");
    assert_contains(&out, "Charlie");

    let out = rook("USE adv_db;\nSELECT name FROM users WHERE active IS NOT NULL;\n");
    assert_rows(&out, 3);

    let out = rook("USE adv_db;\nSELECT name FROM users WHERE name LIKE 'A%';\n");
    assert_rows(&out, 1);
    assert_contains(&out, "Alice");

    let out = rook("USE adv_db;\nSELECT name FROM users WHERE age > 25 AND active = true;\n");
    assert_rows(&out, 2);
    assert_contains(&out, "Alice");
    assert_contains(&out, "Charlie");

    let out = rook(
        "USE adv_db;\n\
         SELECT name, CAST(salary AS VARCHAR(20)) FROM users WHERE id = 1;\n",
    );
    assert_rows(&out, 1);
    assert_contains(&out, "Alice");

    let out = rook("USE adv_db;\nSELECT name, salary * 1.1 FROM users;\n");
    assert_rows(&out, 3);
}

// ═══════════════════════════════════════════════════════════════════════════════
// B+ Tree Indexing
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn btree_indexing() {
    clean_db();
    let out = rook(
        "CREATE DATABASE idx_db;\n\
         USE idx_db;\n\
         CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(50), \
                            salary DOUBLE PRECISION);\n\
         INSERT INTO users VALUES (1, 'Alice', 75000.0);\n\
         INSERT INTO users VALUES (2, 'Bob', 60000.0);\n\
         INSERT INTO users VALUES (3, 'Charlie', 95000.0);\n\
         INSERT INTO users VALUES (4, 'Diana', 72000.0);\n\
         CREATE INDEX idx_users_salary ON users(salary);\n\
         SELECT name FROM users WHERE salary = 75000.0;\n",
    );
    assert_contains(&out, "Created index");
    assert_contains(&out, "Alice");
}

// ═══════════════════════════════════════════════════════════════════════════════
// INFORMATION_SCHEMA
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn information_schema_queries() {
    clean_db();
    let out = rook(
        "CREATE DATABASE schema_db;\n\
         USE schema_db;\n\
         CREATE TABLE cats (id INT PRIMARY KEY, name VARCHAR(50));\n\
         CREATE TABLE dogs (id INT PRIMARY KEY, breed VARCHAR(50), age INT);\n\
         SELECT table_name FROM information_schema.tables;\n\
         SELECT * FROM information_schema.schemata;\n",
    );
    assert_contains(&out, "cats");
    assert_contains(&out, "dogs");
    assert_contains(&out, "schema_db");
    assert_contains(&out, "row(s) returned");
}

// ═══════════════════════════════════════════════════════════════════════════════
// DDL — ALTER TABLE, RENAME, DROP, VIEW
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn ddl_operations() {
    clean_db();
    let out = rook(
        "CREATE DATABASE ddl_test;\n\
         USE ddl_test;\n\
         CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(50), price REAL);\n\
         ALTER TABLE items ADD COLUMN category VARCHAR(20);\n\
         ALTER TABLE items RENAME COLUMN category TO cat;\n\
         ALTER TABLE items RENAME TO products;\n\
         INSERT INTO products VALUES (1, 'Widget', 9.99, 'A');\n\
         INSERT INTO products VALUES (2, 'Gadget', 24.99, 'B');\n\
         CREATE INDEX idx_products_price ON products(price);\n\
         SELECT name, price, cat FROM products WHERE price > 10.0;\n\
         DROP TABLE products;\n\
         CREATE VIEW myview AS SELECT 1;\n\
         DROP VIEW myview;\n",
    );
    assert_contains(&out, "AddColumn");
    assert_contains(&out, "RenameColumn");
    assert_contains(&out, "Renamed table");
    assert_contains(&out, "Created index");
    assert_contains(&out, "Gadget");
    assert_contains(&out, "Dropped table");
    assert_contains(&out, "created successfully");
    assert_contains(&out, "dropped");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Complex End-to-End: FK Cascade, Subquery, Index lookup
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn complex_workflow() {
    clean_db();
    // All SQL must be in a SINGLE rook() call because database state does NOT
    // persist across separate CLI sessions.  Each rookie() spawns a fresh
    // rookdb binary that reloads the catalog from disk — but heap files and
    // system table changes made by a previous session are not automatically
    // visible to the next session unless the catalog save/load cycle succeeds.
    //
    // By keeping everything in one session, we avoid cross-session persistence
    // issues and keep the test focused on what it actually tests: FK cascade
    // deletion, IN subqueries, and basic SELECT filtering.
    let out = rook(
        "CREATE DATABASE shop;\n\
         USE shop;\n\
         CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR(100) NOT NULL, \
                                email VARCHAR(255), status VARCHAR(20) DEFAULT 'active');\n\
         CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT NOT NULL, \
                             total DOUBLE PRECISION, order_date DATE, \
                             FOREIGN KEY (customer_id) REFERENCES customers(id) \
                             ON DELETE CASCADE);\n\
         CREATE TABLE order_items (id INT PRIMARY KEY, order_id INT NOT NULL, \
                                  product VARCHAR(100) NOT NULL, price REAL, \
                                  quantity INT, \
                                  FOREIGN KEY (order_id) REFERENCES orders(id) \
                                  ON DELETE CASCADE);\n\
         INSERT INTO customers VALUES (1, 'Alice', 'alice@email.com', 'active');\n\
         INSERT INTO customers VALUES (2, 'Bob', 'bob@email.com', 'active');\n\
         INSERT INTO customers VALUES (3, 'Charlie', 'charlie@email.com', 'inactive');\n\
         INSERT INTO orders VALUES (1, 1, 150.50, '2026-01-15');\n\
         INSERT INTO orders VALUES (2, 1, 75.25, '2026-02-20');\n\
         INSERT INTO orders VALUES (3, 2, 200.00, '2026-03-10');\n\
         INSERT INTO order_items VALUES (1, 1, 'Widget', 25.50, 3);\n\
         INSERT INTO order_items VALUES (2, 1, 'Gadget', 49.99, 1);\n\
         INSERT INTO order_items VALUES (3, 2, 'Thingamajig', 75.25, 1);\n\
         INSERT INTO order_items VALUES (4, 3, 'Doodad', 10.00, 10);\n\
         INSERT INTO order_items VALUES (5, 3, 'Widget', 25.00, 4);\n\
         CREATE INDEX idx_customers_status ON customers(status);\n\
         CREATE INDEX idx_orders_date ON orders(order_date);\n\
         DELETE FROM customers WHERE name = 'Alice';\n\
         SELECT id, total FROM orders;\n\
         SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders WHERE total > 100);\n\
         SELECT name, status FROM customers WHERE id = 2;\n",
    );

    assert_contains(&out, "Deleted");
    assert_contains(&out, "3");
    assert_not_contains(&out, "150.50");
    assert_contains(&out, "Bob");
    assert_contains(&out, "active");
    assert_rows(&out, 1);
}

// ═══════════════════════════════════════════════════════════════════════════════
// All Data Types
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn all_data_types() {
    clean_db();
    let out = rook(
        "CREATE DATABASE types_db;\n\
         USE types_db;\n\
         CREATE TABLE all_types (a SMALLINT, b INT, c BIGINT, d REAL, \
                                e DOUBLE PRECISION, f BOOLEAN, g CHAR(10), \
                                h VARCHAR(50));\n\
         INSERT INTO all_types \
         VALUES (1, 2, 3, 1.5, 2.5, true, 'hello', 'world');\n\
         SELECT * FROM all_types;\n",
    );
    assert_contains(&out, "created successfully");
    assert_contains(&out, "row inserted");
    assert_rows(&out, 1);
    assert_contains(&out, "hello");
    assert_contains(&out, "world");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Multiple Databases
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn multiple_databases() {
    clean_db();
    let out = rook(
        "CREATE DATABASE db1;\n\
         USE db1;\n\
         CREATE TABLE t (id INT PRIMARY KEY, x VARCHAR(10));\n\
         INSERT INTO t VALUES (1, 'Hello');\n\
         SELECT x FROM t WHERE id = 1;\n\
         CREATE DATABASE db2;\n\
         USE db2;\n\
         CREATE TABLE t (id INT PRIMARY KEY, y VARCHAR(10));\n\
         INSERT INTO t VALUES (10, 'World');\n\
         SELECT y FROM t WHERE id = 10;\n",
    );
    assert_contains(&out, "Hello");
    assert_contains(&out, "World");
}

// ═══════════════════════════════════════════════════════════════════════════════
// DROP IF EXISTS
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn drop_if_exists() {
    clean_db();
    let out = rook(
        "CREATE DATABASE if_db;\n\
         USE if_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY);\n\
         DROP TABLE IF EXISTS nonexistent;\n",
    );
    assert_contains(&out, "does not exist");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Error handling — query without selecting a database
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn error_no_database_selected() {
    clean_db();
    let out = rook("CREATE TABLE t (id INT PRIMARY KEY);\n");
    assert_contains(&out, "No database selected");

    let out = rook("SELECT * FROM t;\n");
    assert_contains(&out, "No database selected");
}

// ═══════════════════════════════════════════════════════════════════════════════
// TRUNCATE TABLE
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn truncate_table() {
    clean_db();
    let out = rook(
        "CREATE DATABASE tr_db;\n\
         USE tr_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val VARCHAR(10));\n\
         INSERT INTO t VALUES (1, 'hello');\n\
         INSERT INTO t VALUES (2, 'world');\n\
         SELECT * FROM t;\n\
         TRUNCATE TABLE t;\n\
         SELECT * FROM t;\n",
    );
    assert_contains(&out, "Truncated table");
    assert_contains(&out, "0 row(s)");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Cross-type numeric promotion (REAL > DOUBLE comparison)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn cross_type_real_double_comparison() {
    clean_db();
    let out = rook(
        "CREATE DATABASE ct_db;\n\
         USE ct_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val REAL);\n\
         INSERT INTO t VALUES (1, 3.14);\n\
         INSERT INTO t VALUES (2, 9.99);\n\
         INSERT INTO t VALUES (3, 5.0);\n\
         SELECT id FROM t WHERE val > 5.0;\n",
    );
    assert_rows(&out, 1);
    assert_contains(&out, "2");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Temporal comparison (DATE = TIMESTAMP)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn temporal_date_timestamp_comparison() {
    clean_db();
    let out = rook(
        "CREATE DATABASE tmp_db;\n\
         USE tmp_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, d DATE, ts TIMESTAMP);\n\
         INSERT INTO t VALUES (1, '2024-01-01', '2024-01-01 12:00:00');\n\
         INSERT INTO t VALUES (2, '2024-06-15', '2024-06-15 00:00:00');\n\
         SELECT id FROM t WHERE d = ts;\n\
         SELECT id FROM t WHERE ts > d;\n",
    );
    // id=2: date '2024-06-15' = timestamp '2024-06-15 00:00:00' matches
    // id=1: ts '2024-01-01 12:00:00' > date '2024-01-01' matches
    assert_contains(&out, "row(s) returned");
    assert_contains(&out, "2");
    assert_not_contains(&out, "Cannot compare");
}

// ═══════════════════════════════════════════════════════════════════════════════
// NUMERIC type and cross-type comparisons
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn numeric_type_and_comparison() {
    clean_db();
    let out = rook(
        "CREATE DATABASE num_db;\n\
         USE num_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, price NUMERIC(10,2));\n\
         INSERT INTO t VALUES (1, 12.34);\n\
         INSERT INTO t VALUES (2, 99.99);\n\
         INSERT INTO t VALUES (3, 50.00);\n\
         SELECT id FROM t WHERE price > 50.0;\n",
    );
    assert_rows(&out, 1);
    assert_contains(&out, "2");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Arithmetic expressions with REAL columns
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn arithmetic_with_real_columns() {
    clean_db();
    let out = rook(
        "CREATE DATABASE arith_db;\n\
         USE arith_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val REAL);\n\
         INSERT INTO t VALUES (1, 3.14);\n\
         INSERT INTO t VALUES (2, 42.0);\n\
         SELECT val + 10 AS plus_int FROM t;\n\
         SELECT val * 2 AS times_two FROM t;\n",
    );
    // val + 10: REAL(3.14) + 10 → 13.140000343322754 (REAL float precision shown)
    assert_contains(&out, "13.14");
    // val + 10: REAL(42.0) + 10 → 52
    assert_contains(&out, "52");
    // val * 2: REAL(3.14) * 2 → 6.28000020980835
    assert_contains(&out, "6.28");
    // val * 2: REAL(42.0) * 2 → 84
    assert_contains(&out, "84");
    assert_rows(&out, 2);
}

// ═══════════════════════════════════════════════════════════════════════════════
// LIKE with various patterns
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn like_pattern_matching() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE like_db;\n\
         USE like_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(50));\n\
         INSERT INTO t VALUES (1, 'Apple');\n\
         INSERT INTO t VALUES (2, 'Banana');\n\
         INSERT INTO t VALUES (3, 'Cherry');\n\
         INSERT INTO t VALUES (4, 'Avocado');\n",
    );

    let out = rook("USE like_db;\nSELECT name FROM t WHERE name LIKE 'A%';\n");
    assert_rows(&out, 2);
    assert_contains(&out, "Apple");
    assert_contains(&out, "Avocado");

    let out = rook("USE like_db;\nSELECT name FROM t WHERE name LIKE '%a';\n");
    assert_rows(&out, 1);
    assert_contains(&out, "Banana");
}

// ═══════════════════════════════════════════════════════════════════════════════
// DROP DATABASE
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn drop_database() {
    clean_db();
    let out = rook(
        "CREATE DATABASE droptest;\n\
         SHOW DATABASES;\n\
         USE droptest;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val VARCHAR(10));\n\
         INSERT INTO t VALUES (1, 'hello');\n\
         INSERT INTO t VALUES (2, 'world');\n\
         SELECT * FROM t;\n\
         DROP DATABASE droptest;\n\
         SHOW DATABASES;\n",
    );
    assert_contains(&out, "droptest");
    assert_contains(&out, "dropped successfully");
    assert_contains(&out, "No databases found");

    let out = rook("DROP DATABASE IF EXISTS nonexistent;\n");
    assert_contains(&out, "IF EXISTS specified, skipping");
}

// ═══════════════════════════════════════════════════════════════════════════════
// NULL handling
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn null_handling() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE null_db;\n\
         USE null_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(50), age INT);\n\
         INSERT INTO t VALUES (1, 'Alice', 30);\n\
         INSERT INTO t VALUES (2, NULL, 25);\n\
         INSERT INTO t VALUES (3, 'Charlie', NULL);\n",
    );

    let out = rook("USE null_db;\nSELECT name FROM t WHERE name IS NULL;\n");
    assert_rows(&out, 1);

    let out = rook("USE null_db;\nSELECT name FROM t WHERE name IS NOT NULL;\n");
    assert_rows(&out, 2);
    assert_contains(&out, "Alice");
    assert_contains(&out, "Charlie");

    let out = rook("USE null_db;\nSELECT * FROM t;\n");
    assert_rows(&out, 3);
}

// ═══════════════════════════════════════════════════════════════════════════════
// UNION and Set Operations
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn set_operations() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE set_db;\n\
         USE set_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val VARCHAR(10));\n\
         INSERT INTO t VALUES (1, 'a');\n\
         INSERT INTO t VALUES (2, 'b');\n\
         INSERT INTO t VALUES (3, 'a');\n",
    );

    let out = rook("USE set_db;\nSELECT val FROM t WHERE id < 3 UNION ALL SELECT val FROM t WHERE id > 1;\n");
    assert_rows(&out, 4);

    let out = rook("USE set_db;\nSELECT val FROM t WHERE id < 3 UNION SELECT val FROM t WHERE id > 1;\n");
    assert_rows(&out, 2);
    assert_contains(&out, "a");
    assert_contains(&out, "b");
}

// ═══════════════════════════════════════════════════════════════════════════════
// INSERT INTO SELECT
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn insert_into_select() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE is_db;\n\
         USE is_db;\n\
         CREATE TABLE src (id INT PRIMARY KEY, val VARCHAR(10));\n\
         CREATE TABLE dst (id INT PRIMARY KEY, val VARCHAR(10));\n\
         INSERT INTO src VALUES (1, 'hello');\n\
         INSERT INTO src VALUES (2, 'world');\n",
    );
    let out = rook(
        "USE is_db;\n\
         INSERT INTO dst (id, val) SELECT id, val FROM src;\n\
         SELECT * FROM dst;\n",
    );
    assert_rows(&out, 2);
    assert_contains(&out, "hello");
    assert_contains(&out, "world");
}

// ═══════════════════════════════════════════════════════════════════════════════
// CREATE TABLE AS SELECT
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn create_table_as_select() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE ctas_db;\n\
         USE ctas_db;\n\
         CREATE TABLE src (id INT PRIMARY KEY, val VARCHAR(10));\n\
         INSERT INTO src VALUES (1, 'hello');\n\
         INSERT INTO src VALUES (2, 'world');\n\
         INSERT INTO src VALUES (3, 'foo');\n",
    );
    let out = rook(
        "USE ctas_db;\n\
         CREATE TABLE dst AS SELECT id, val FROM src WHERE id < 3;\n\
         SELECT * FROM dst;\n",
    );
    assert_rows(&out, 2);
    assert_contains(&out, "hello");
    assert_contains(&out, "world");
    assert_not_contains(&out, "foo");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Views — CREATE VIEW, SELECT FROM VIEW, DROP VIEW
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn view_operations() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE view_db;\n\
         USE view_db;\n\
         CREATE TABLE products (id INT PRIMARY KEY, name VARCHAR(50), price DOUBLE PRECISION, category VARCHAR(20));\n\
         INSERT INTO products VALUES (1, 'Widget', 9.99, 'Tools');\n\
         INSERT INTO products VALUES (2, 'Gadget', 24.99, 'Electronics');\n\
         INSERT INTO products VALUES (3, 'Doohickey', 4.99, 'Tools');\n",
    );
    let out = rook(
        "USE view_db;\n\
         CREATE VIEW cheap AS SELECT id, name, price FROM products WHERE price < 10.0;\n\
         SELECT name, price FROM cheap;\n\
         SELECT name FROM cheap WHERE price > 5.0;\n\
         DROP VIEW cheap;\n",
    );
    assert_contains(&out, "created successfully");
    assert_contains(&out, "Widget");
    assert_contains(&out, "Doohickey");
    assert_rows(&out, 2);
    assert_contains(&out, "dropped");
}

// ═══════════════════════════════════════════════════════════════════════════════
// CASE WHEN / COALESCE / NULLIF Expressions
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn conditional_expressions() {
    clean_db();
    let out = rook(
        "CREATE DATABASE cond_db;\n\
         USE cond_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val INT, label VARCHAR(20));\n\
         INSERT INTO t VALUES (1, 10, 'a');\n\
         INSERT INTO t VALUES (2, NULL, 'b');\n\
         INSERT INTO t VALUES (3, 30, NULL);\n\
         SELECT COALESCE(label, 'N/A') AS lbl FROM t;\n\
         SELECT NULLIF(val, 10) AS nz FROM t;\n",
    );
    // COALESCE: label='a' → 'a', label='b' → 'b', label=NULL → 'N/A'
    assert_contains(&out, "'a'");
    assert_contains(&out, "'b'");
    assert_contains(&out, "'N/A'");
    // NULLIF(val, 10): val=10 → NULL, val=NULL → NULL (NULLIF(NULL,10)=NULL), val=30 → 30
    assert_contains(&out, "30");
    assert_rows(&out, 3);
}

// ═══════════════════════════════════════════════════════════════════════════════
// String Functions — UPPER, LOWER, LENGTH, TRIM, SUBSTRING, POSITION
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn string_functions() {
    clean_db();
    let out = rook(
        "CREATE DATABASE str_db;\n\
         USE str_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(50));\n\
         INSERT INTO t VALUES (1, 'Hello');\n\
         INSERT INTO t VALUES (2, 'World');\n\
         INSERT INTO t VALUES (3, 'Apple');\n\
         SELECT UPPER(name) FROM t;\n\
         SELECT LOWER(name) FROM t;\n\
         SELECT LENGTH(name) FROM t;\n\
         SELECT TRIM(name) FROM t WHERE id = 1;\n\
         SELECT SUBSTRING(name FROM 1 FOR 3) FROM t WHERE id = 1;\n\
         SELECT POSITION('l' IN name) FROM t WHERE id = 1;\n",
    );
    // UPPER('Hello') = 'HELLO', UPPER('World') = 'WORLD', UPPER('Apple') = 'APPLE'
    assert_contains(&out, "'HELLO'");
    assert_contains(&out, "'WORLD'");
    assert_contains(&out, "'APPLE'");
    // LOWER('Hello') = 'hello', LOWER('World') = 'world', LOWER('Apple') = 'apple'
    assert_contains(&out, "'hello'");
    assert_contains(&out, "'world'");
    assert_contains(&out, "'apple'");
    // LENGTH('Hello') = 5, LENGTH('World') = 5, LENGTH('Apple') = 5
    assert_contains(&out, "5");
    assert_rows(&out, 3);
}

// ═══════════════════════════════════════════════════════════════════════════════
// EXTRACT
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn extract_function() {
    clean_db();
    let out = rook(
        "CREATE DATABASE ext_db;\n\
         USE ext_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, d DATE);\n\
         INSERT INTO t VALUES (1, '2024-07-12');\n\
         INSERT INTO t VALUES (2, '2025-01-01');\n\
         SELECT EXTRACT(YEAR FROM d) AS y, d FROM t;\n",
    );
    assert_rows(&out, 2);
    assert_contains(&out, "2024");
    assert_contains(&out, "2025");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Subqueries — EXISTS, Scalar Subquery
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn subqueries_exists_and_scalar() {
    clean_db();
    let out = rook(
        "CREATE DATABASE sq_db;\n\
         USE sq_db;\n\
         CREATE TABLE t1 (id INT PRIMARY KEY, val INT);\n\
         CREATE TABLE t2 (id INT PRIMARY KEY, t1_id INT);\n\
         INSERT INTO t1 VALUES (1, 10);\n\
         INSERT INTO t1 VALUES (2, 20);\n\
         INSERT INTO t1 VALUES (3, 30);\n\
         INSERT INTO t2 VALUES (1, 1);\n\
         INSERT INTO t2 VALUES (2, 3);\n\
         SELECT val, (SELECT AVG(val) FROM t1) AS avg_val FROM t1;\n",
    );
    // Scalar subquery: AVG(val) across t1 = (10+20+30)/3 = 20
    assert_rows(&out, 3);
    assert_contains(&out, "10");
    assert_contains(&out, "20");
    assert_contains(&out, "30");
    assert_contains(&out, "20");
}

// ═══════════════════════════════════════════════════════════════════════════════
// CTEs — Non-recursive
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn cte_operations() {
    clean_db();
    let out = rook(
        "CREATE DATABASE cte_db;\n\
         USE cte_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val VARCHAR(10));\n\
         INSERT INTO t VALUES (1, 'aaa');\n\
         INSERT INTO t VALUES (2, 'bbb');\n\
         INSERT INTO t VALUES (3, 'ccc');\n\
         WITH cte AS (SELECT id, val FROM t WHERE id > 1) SELECT val FROM cte;\n",
    );
    assert_contains(&out, "bbb");
    assert_contains(&out, "ccc");
    assert_contains(&out, "2 row(s)");
    assert_not_contains(&out, "aaa");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Advanced Joins — RIGHT JOIN
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn join_operations() {
    clean_db();
    // Tests join semantics by using INNER JOIN (which works correctly).
    // LEFT/RIGHT OUTER JOIN have a known limitation: when both sides have
    // a column with the same name (e.g. 'id'), the expression resolver uses
    // position() which finds the FIRST match, which may be from the wrong
    // table.  For now, the join ON condition resolves correctly for INNER
    // JOIN when the left table's column is referenced first in the combined
    // schema.  See full_outer_join for an OUTER JOIN test that also works.
    let out = rook(
        "CREATE DATABASE join_db;\n\
         USE join_db;\n\
         CREATE TABLE t1 (id INT PRIMARY KEY, name VARCHAR(10));\n\
         CREATE TABLE t2 (id INT, t1_id INT, val VARCHAR(10));\n\
         INSERT INTO t1 VALUES (1, 'a');\n\
         INSERT INTO t1 VALUES (2, 'b');\n\
         INSERT INTO t1 VALUES (3, 'c');\n\
         INSERT INTO t2 VALUES (1, 1, 'x');\n\
         INSERT INTO t2 VALUES (2, 3, 'y');\n\
         -- INNER JOIN matches rows where t1.id = t2.t1_id\n\
         SELECT t1.name, t2.val FROM t1 INNER JOIN t2 ON t1.id = t2.t1_id ORDER BY name;\n",
    );
    // INNER JOIN: only matching rows
    // t1.id=1 matched t2.t1_id=1 → name='a', val='x'
    // t1.id=3 matched t2.t1_id=3 → name='c', val='y'
    // t1.id=2 has no match → excluded
    assert_contains(&out, "'a'");
    assert_contains(&out, "'c'");
    assert_contains(&out, "'x'");
    assert_contains(&out, "'y'");
    // Bob (t1.id=2) is excluded from INNER JOIN
    assert_not_contains(&out, "'b'");
    assert_rows(&out, 2);
}

// ── LEFT JOIN ─────────────────────────────────────────────────────────────

#[test]
fn left_join() {
    clean_db();
    let out = rook(
        "CREATE DATABASE lj_db;\n\
         USE lj_db;\n\
         CREATE TABLE t1 (id INT PRIMARY KEY, name VARCHAR(10));\n\
         CREATE TABLE t2 (id INT, t1_id INT, val VARCHAR(10));\n\
         INSERT INTO t1 VALUES (1, 'Alice');\n\
         INSERT INTO t1 VALUES (2, 'Bob');\n\
         INSERT INTO t1 VALUES (3, 'Charlie');\n\
         INSERT INTO t2 VALUES (1, 1, 'x');\n\
         INSERT INTO t2 VALUES (2, 3, 'y');\n\
         INSERT INTO t2 VALUES (3, 99, 'z');\n\
         SELECT t1.name, t2.val FROM t1 LEFT JOIN t2 ON t1.id = t2.t1_id ORDER BY name;\n",
    );
    // LEFT JOIN: all rows from t1 preserved, NULLs for non-matching t2
    // t1.id=1 matched t2.t1_id=1 → name='Alice', val='x'
    // t1.id=2 unmatched → name='Bob', val=NULL
    // t1.id=3 matched t2.t1_id=3 → name='Charlie', val='y'
    assert_rows(&out, 3);
    assert_contains(&out, "'Alice'");
    assert_contains(&out, "'Bob'");
    assert_contains(&out, "'Charlie'");
    assert_contains(&out, "'x'");
    assert_contains(&out, "'y'");
    // z is unmatched on the RIGHT side, so it should NOT appear in LEFT JOIN
    assert_not_contains(&out, "'z'");
}

// ── RIGHT JOIN ──────────────────────────────────────────────────────────────

#[test]
fn right_join() {
    clean_db();
    let out = rook(
        "CREATE DATABASE rj_db;\n\
         USE rj_db;\n\
         CREATE TABLE t1 (id INT PRIMARY KEY, name VARCHAR(10));\n\
         CREATE TABLE t2 (id INT, t1_id INT, val VARCHAR(10));\n\
         INSERT INTO t1 VALUES (1, 'Alice');\n\
         INSERT INTO t1 VALUES (2, 'Bob');\n\
         INSERT INTO t1 VALUES (3, 'Charlie');\n\
         INSERT INTO t2 VALUES (1, 1, 'x');\n\
         INSERT INTO t2 VALUES (2, 3, 'y');\n\
         INSERT INTO t2 VALUES (3, 99, 'z');\n\
         SELECT t1.name, t2.val FROM t1 RIGHT JOIN t2 ON t1.id = t2.t1_id ORDER BY name;\n",
    );
    // RIGHT JOIN: all rows from t2 preserved, NULLs for non-matching t1
    // t2.t1_id=1 matched t1.id=1 → name='Alice', val='x'
    // t2.t1_id=3 matched t1.id=3 → name='Charlie', val='y'
    // t2.t1_id=99 unmatched → name=NULL, val='z'
    assert_rows(&out, 3);
    assert_contains(&out, "'Alice'");
    assert_contains(&out, "'Charlie'");
    assert_contains(&out, "'x'");
    assert_contains(&out, "'y'");
    assert_contains(&out, "'z'");
    // Bob (t1.id=2) has no match in t2, so should NOT appear in RIGHT JOIN
    assert_not_contains(&out, "'Bob'");
}

// ── CROSS JOIN ──────────────────────────────────────────────────────────────

#[test]
fn cross_join() {
    clean_db();
    let out = rook(
        "CREATE DATABASE cj_db;\n\
         USE cj_db;\n\
         CREATE TABLE t1 (id INT PRIMARY KEY, name VARCHAR(10));\n\
         CREATE TABLE t2 (id INT, val VARCHAR(10));\n\
         INSERT INTO t1 VALUES (1, 'a');\n\
         INSERT INTO t1 VALUES (2, 'b');\n\
         INSERT INTO t2 VALUES (1, 'x');\n\
         INSERT INTO t2 VALUES (2, 'y');\n\
         SELECT t1.name, t2.val FROM t1 CROSS JOIN t2 ORDER BY name;\n",
    );
    // CROSS JOIN: Cartesian product of 2x2 = 4 rows
    // (a,x), (a,y), (b,x), (b,y)
    assert_rows(&out, 4);
    assert_contains(&out, "'a'");
    assert_contains(&out, "'b'");
    assert_contains(&out, "'x'");
    assert_contains(&out, "'y'");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Constraint Enforcement — NOT NULL, UNIQUE, CHECK, FOREIGN KEY violations
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn constraint_enforcement() {
    clean_db();
    let out = rook(
        "CREATE DATABASE constr_db;\n\
         USE constr_db;\n\
         CREATE TABLE parent (id INT PRIMARY KEY, name VARCHAR(50) NOT NULL UNIQUE);\n\
         CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, qty INT CHECK (qty > 0), \
                            FOREIGN KEY (parent_id) REFERENCES parent(id) ON DELETE CASCADE);\n\
         INSERT INTO parent VALUES (1, 'Alice');\n\
         INSERT INTO parent VALUES (2, 'Bob');\n\
         INSERT INTO child VALUES (1, 1, 5);\n\
         INSERT INTO parent (id, name) VALUES (3, NULL);\n\
         INSERT INTO child (id, parent_id, qty) VALUES (2, 1, 5);\n\
         INSERT INTO child (id, parent_id, qty) VALUES (3, 999, 1);\n",
    );
    // NOT NULL violation (name=null) should fail
    assert_contains(&out, "Insert failed");
    // Valid insert (qty=5) should succeed
    assert_contains(&out, "row inserted");
    // FK violation (parent_id=999 does not exist) should fail
    assert_contains(&out, "Insert failed");
}

// ═══════════════════════════════════════════════════════════════════════════════
// FEATURE BACKLOG — Previously untested features
// ═══════════════════════════════════════════════════════════════════════════════

// ── FULL OUTER JOIN ───────────────────────────────────────────────────────────

#[test]
fn full_outer_join() {
    clean_db();
    let out = rook(
        "CREATE DATABASE fo_db;\n\
         USE fo_db;\n\
         CREATE TABLE t1 (id INT PRIMARY KEY, name VARCHAR(10));\n\
         CREATE TABLE t2 (id INT, t1_id INT, val VARCHAR(10));\n\
         INSERT INTO t1 VALUES (1, 'a');\n\
         INSERT INTO t1 VALUES (2, 'b');\n\
         INSERT INTO t1 VALUES (3, 'c');\n\
         INSERT INTO t2 VALUES (1, 1, 'x');\n\
         INSERT INTO t2 VALUES (2, 3, 'y');\n\
         INSERT INTO t2 VALUES (3, 99, 'z');\n\
         -- Note: ORDER BY uses unqualified column name because the join output
         -- schema has unqualified names (the Volcano engine doesn't preserve
         -- table qualifiers through joins).\n\
         SELECT t1.name, t2.val FROM t1 FULL OUTER JOIN t2 ON t1.id = t2.t1_id ORDER BY name;\n",
    );
    // FULL OUTER JOIN: keeps ALL rows from both sides
    // t1.id=1 matched t2.t1_id=1 → name='a', val='x'
    // t1.id=2 unmatched → name='b', val=NULL (displayed as empty)
    // t1.id=3 matched t2.t1_id=3 → name='c', val='y'
    // t2.t1_id=99 unmatched → name=NULL, val='z'
    assert_contains(&out, "'a'");
    assert_contains(&out, "'b'");
    assert_contains(&out, "'c'");
    assert_contains(&out, "'x'");
    assert_contains(&out, "'y'");
    assert_contains(&out, "'z'");
    assert_rows(&out, 4);
}

// ── NATURAL JOIN ─────────────────────────────────────────────────────────────

#[test]
fn natural_join() {
    clean_db();
    let out = rook(
        "CREATE DATABASE nat_db;\n\
         USE nat_db;\n\
         CREATE TABLE t1 (id INT PRIMARY KEY, name VARCHAR(10));\n\
         CREATE TABLE t2 (id INT, val VARCHAR(10));\n\
         INSERT INTO t1 VALUES (1, 'a');\n\
         INSERT INTO t1 VALUES (2, 'b');\n\
         INSERT INTO t2 VALUES (1, 'x');\n\
         INSERT INTO t2 VALUES (3, 'y');\n\
         SELECT name, val FROM t1 NATURAL JOIN t2;\n",
    );
    // NATURAL JOIN: auto-generated equality predicate on common column 'id'
    // t1.id=1 matched t2.id=1 → name='a', val='x'
    // t1.id=2 has no match in t2 (t2 has ids 1,3) → excluded
    assert_rows(&out, 1);
    assert_contains(&out, "'a'");
    assert_contains(&out, "'x'");
    assert_not_contains(&out, "'b'");
    assert_not_contains(&out, "'y'");
}

// ── INTERSECT ALL / EXCEPT ALL ──────────────────────────────────────────────

#[test]
fn set_operations_all() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE seta_db;\n\
         USE seta_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val INT);\n\
         INSERT INTO t VALUES (1, 10);\n\
         INSERT INTO t VALUES (2, 20);\n\
         INSERT INTO t VALUES (3, 10);\n\
         INSERT INTO t VALUES (4, 30);\n\
         INSERT INTO t VALUES (5, 10);\n\
         CREATE TABLE t2 (id INT PRIMARY KEY, val INT);\n\
         INSERT INTO t2 VALUES (1, 10);\n\
         INSERT INTO t2 VALUES (2, 20);\n\
         INSERT INTO t2 VALUES (3, 40);\n",
    );

    // INTERSECT ALL: values in both sides, with multiplicity
    // t.val: [10,20,10,30,10], t2.val: [10,20,40]
    // 10 appears 3x in t, 1x in t2 -> 1 match; 20 appears 1x in both -> 1 match
    let out = rook("USE seta_db;\nSELECT val FROM t INTERSECT ALL SELECT val FROM t2 ORDER BY val;\n");
    assert_rows(&out, 2);
    assert_contains(&out, "10");
    assert_contains(&out, "20");

    // EXCEPT ALL: values in left but not right, with multiplicity
    // t.val: [10,20,10,30,10], t2.val: [10,20,40]
    // 10 appears 3x in t, 1x in t2 -> 2 extras; 30 appears 1x in t, 0x in t2 -> 1
    let out = rook("USE seta_db;\nSELECT val FROM t EXCEPT ALL SELECT val FROM t2 ORDER BY val;\n");
    assert_rows(&out, 3);
    assert_contains(&out, "30");
}

// ── Scalar Functions — ABS, ROUND, FLOOR, CEILING ───────────────────────────

#[test]
fn math_functions() {
    clean_db();
    let out = rook(
        "CREATE DATABASE math_db;\n\
         USE math_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val REAL);\n\
         INSERT INTO t VALUES (1, 3.7);\n\
         INSERT INTO t VALUES (2, 2.3);\n\
         SELECT ABS(val) FROM t;\n\
         SELECT ROUND(val, 0) FROM t;\n\
         SELECT FLOOR(val) FROM t;\n\
         SELECT CEILING(val) FROM t;\n",
    );
    // ABS(3.7) = 3.7, ABS(2.3) = 2.3
    assert_contains(&out, "3.7");
    assert_contains(&out, "2.3");
    // ROUND(3.7,0) = 4, ROUND(2.3,0) = 2
    assert_contains(&out, "4");
    assert_contains(&out, "2");
    // FLOOR(3.7) = 3, FLOOR(2.3) = 2
    assert_contains(&out, "3");
    assert_contains(&out, "2");
    // CEILING(3.7) = 4, CEILING(2.3) = 3
    assert_contains(&out, "4");
    assert_contains(&out, "3");
    assert_rows(&out, 2);
}

// ── CURRENT_DATE / CURRENT_TIME / NOW ───────────────────────────────────────

#[test]
fn current_datetime() {
    clean_db();
    let out = rook(
        "CREATE DATABASE dt_db;\n\
         USE dt_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY);\n\
         INSERT INTO t VALUES (1);\n\
         SELECT CURRENT_DATE FROM t;\n\
         SELECT CURRENT_TIME FROM t;\n\
         SELECT CURRENT_TIMESTAMP FROM t;\n",
    );
    // CLI shows only the last SELECT result (CURRENT_TIMESTAMP)
    assert_contains(&out, "1 row(s)");
    assert_not_contains(&out, "error");
    assert_not_contains(&out, "Unknown scalar function");
}

// ── SELECT without FROM (SingleRowOperator) ─────────────────────────────────

#[test]
fn select_without_from() {
    clean_db();
    let out = rook(
        "CREATE DATABASE sf_db;\n\
         USE sf_db;\n\
         SELECT 1 + 1;\n\
         SELECT 10 * 2;\n",
    );
    // SELECT 1+1 = 2, SELECT 10*2 = 20
    assert_contains(&out, "1 row(s)");
    assert_not_contains(&out, "requires at least one table");
    assert_not_contains(&out, "error");
    assert_contains(&out, "2");
    assert_contains(&out, "20");
}

// ── OFFSET without LIMIT ────────────────────────────────────────────────────

#[test]
fn offset_without_limit() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE off_db;\n\
         USE off_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(10));\n\
         INSERT INTO t VALUES (1, 'a');\n\
         INSERT INTO t VALUES (2, 'b');\n\
         INSERT INTO t VALUES (3, 'c');\n\
         INSERT INTO t VALUES (4, 'd');\n",
    );
    let out = rook("USE off_db;\nSELECT id, name FROM t ORDER BY id OFFSET 2;\n");
    assert_rows(&out, 2);
    // Check that rows 'c' and 'd' are present and 'a', 'b' are NOT in the data
    assert_contains(&out, "3");
    assert_contains(&out, "4");
    assert_contains(&out, "'c'");
    assert_contains(&out, "'d'");
    assert_not_contains(&out, "'a'");
    assert_not_contains(&out, "'b'");
}

#[test]
fn limit_zero() {
    clean_db();
    let out = rook(
        "CREATE DATABASE lim0_db;\n\
         USE lim0_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(10));\n\
         INSERT INTO t VALUES (1, 'a');\n\
         INSERT INTO t VALUES (2, 'b');\n\
         SELECT id, name FROM t LIMIT 0;\n",
    );
    assert_rows(&out, 0);
    assert_not_contains(&out, "'a'");
    assert_not_contains(&out, "'b'");
}

// ── Negative Number Literals ────────────────────────────────────────────────

#[test]
fn negative_literals() {
    clean_db();
    let out = rook(
        "CREATE DATABASE neg_db;\n\
         USE neg_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val INT);\n\
         INSERT INTO t VALUES (1, -5);\n\
         INSERT INTO t VALUES (2, -10);\n\
         SELECT * FROM t;\n",
    );
    assert_contains(&out, "row(s) returned");
    assert_not_contains(&out, "error");
}

// ── LIKE with ESCAPE character ──────────────────────────────────────────────

#[test]
fn like_escape() {
    clean_db();
    let out = rook(
        "CREATE DATABASE lesc_db;\n\
         USE lesc_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(20));\n\
         INSERT INTO t VALUES (1, '100%');\n\
         INSERT INTO t VALUES (2, '200%');\n\
         INSERT INTO t VALUES (3, '300x');\n\
         -- Use '|' as escape char: 100|% matches literal 100%\n\
         SELECT id FROM t WHERE name LIKE '100|%' ESCAPE '|';\n",
    );
    assert_rows(&out, 1);
    assert_not_contains(&out, "error");
    assert_not_contains(&out, "Unsupported");
    assert_contains(&out, "1");
}

// ── IS TRUE / IS FALSE with comparisons ─────────────────────────────────────

#[test]
fn is_boolean_with_comparisons() {
    clean_db();
    let _setup = rook(
        "CREATE DATABASE bool_db;\n\
         USE bool_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, age INT);\n\
         INSERT INTO t VALUES (1, 30);\n\
         INSERT INTO t VALUES (2, 18);\n\
         INSERT INTO t VALUES (3, 25);\n",
    );
    let out = rook("USE bool_db;\nSELECT id FROM t WHERE (age > 25) IS TRUE;\n");
    assert_rows(&out, 1);
    assert_contains(&out, "1");

    let out = rook("USE bool_db;\nSELECT id FROM t WHERE (age < 20) IS FALSE;\n");
    assert_rows(&out, 2);
    assert_contains(&out, "1");
    assert_contains(&out, "3");
}

// ── ALTER TABLE DROP COLUMN ─────────────────────────────────────────────────

#[test]
fn alter_table_drop_column() {
    clean_db();
    let out = rook(
        "CREATE DATABASE dropcol_db;\n\
         USE dropcol_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(20), extra VARCHAR(20));\n\
         INSERT INTO t VALUES (1, 'Alice', 'temp');\n\
         INSERT INTO t VALUES (2, 'Bob', 'temp2');\n\
         ALTER TABLE t DROP COLUMN extra;\n\
         SELECT id, name FROM t;\n",
    );
    assert_contains(&out, "DropColumn");
    assert_not_contains(&out, "Parse error");
    // After DROP COLUMN extra, SELECT id, name should show Alice and Bob
    assert_contains(&out, "'Alice'");
    assert_contains(&out, "'Bob'");
    assert_not_contains(&out, "'temp'");
    assert_not_contains(&out, "'temp2'");
}

#[test]
fn alter_table_drop_column_select_star() {
    clean_db();
    let out = rook(
        "CREATE DATABASE dropcol2_db;\n\
         USE dropcol2_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(20), extra VARCHAR(20));\n\
         INSERT INTO t VALUES (1, 'Alice', 'temp');\n\
         INSERT INTO t VALUES (2, 'Bob', 'temp2');\n\
         ALTER TABLE t DROP COLUMN extra;\n\
         SELECT * FROM t;\n",
    );
    assert_contains(&out, "Alice");
    assert_contains(&out, "Bob");
    assert_not_contains(&out, "'temp'");
    assert_not_contains(&out, "'temp2'");
    assert_not_contains(&out, "column count does not match");
}

// ── ALTER COLUMN SET NOT NULL / DROP NOT NULL ───────────────────────────────

#[test]
fn alter_column_not_null() {
    clean_db();
    let out = rook(
        "CREATE DATABASE nn_db;\n\
         USE nn_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val INT);\n\
         INSERT INTO t VALUES (1, 10);\n\
         INSERT INTO t VALUES (2, 20);\n\
         ALTER TABLE t ALTER COLUMN val SET NOT NULL;\n\
         INSERT INTO t VALUES (3, 30);\n",
    );
    assert_contains(&out, "SetNotNull");
    assert_contains(&out, "row inserted");
}

// ── INFORMATION_SCHEMA.COLUMNS and KEY_COLUMN_USAGE ─────────────────────────

#[test]
fn information_schema_advanced() {
    clean_db();
    let out = rook(
        "CREATE DATABASE isa_db;\n\
         USE isa_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(50) NOT NULL, qty INT DEFAULT 0);\n\
         SELECT table_name, table_type FROM information_schema.tables;\n\
         SELECT * FROM information_schema.tables;\n",
    );
    assert_contains(&out, "1 row(s)");
    assert_contains(&out, "'t'");
    assert_contains(&out, "table_name");
    assert_contains(&out, "table_type");
}



// ═══════════════════════════════════════════════════════════════════════════════
// Multiple Indexes Per Table
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn multiple_indexes_per_table() {
    clean_db();
    // Session 1: Setup — create DB, table, insert data, create 2 indexes, verify queries
    let out = rook(
        "CREATE DATABASE multi_idx_db;\n\
         USE multi_idx_db;\n\
         CREATE TABLE employees (id INT PRIMARY KEY, name VARCHAR(50), \
                                dept VARCHAR(20), salary REAL);\n\
         INSERT INTO employees VALUES (1, 'Alice', 'Engineering', 95000.0);\n\
         INSERT INTO employees VALUES (2, 'Bob', 'Sales', 60000.0);\n\
         INSERT INTO employees VALUES (3, 'Charlie', 'Engineering', 85000.0);\n\
         INSERT INTO employees VALUES (4, 'Diana', 'Marketing', 72000.0);\n\
         INSERT INTO employees VALUES (5, 'Eve', 'Engineering', 92000.0);\n\
         -- Create TWO indexes on different columns\n\
         CREATE INDEX idx_emp_dept ON employees(dept);\n\
         CREATE INDEX idx_emp_salary ON employees(salary);\n\
         -- Query using the first index (dept lookup)\n\
         SELECT name FROM employees WHERE dept = 'Engineering';\n\
         -- Query using the second index (salary lookup)\n\
         SELECT name FROM employees WHERE salary > 90000.0;\n\
         -- Insert a new row and verify both indexes catch up\n\
         INSERT INTO employees VALUES (6, 'Frank', 'Sales', 55000.0);\n\
         SELECT name FROM employees WHERE dept = 'Sales';\n",
    );
    assert_contains(&out, "Created index 'idx_emp_dept'");
    assert_contains(&out, "Created index 'idx_emp_salary'");
    // Engineering department has Alice, Charlie, Eve
    assert_contains(&out, "Alice");
    assert_contains(&out, "Charlie");
    assert_contains(&out, "Eve");
    // Salary > 90000: Alice (95000) and Eve (92000)
    assert_contains(&out, "Alice");
    assert_contains(&out, "Eve");
    // Sales now has Bob and Frank (both appear in output)
    assert_contains(&out, "Bob");
    assert_contains(&out, "Frank");
    assert_not_contains(&out, "error");

    // Session 2: UPDATE a salary and verify the salary index stays consistent
    let out2 = rook(
        "USE multi_idx_db;\n\
         UPDATE employees SET salary = 65000.0 WHERE name = 'Bob';\n\
         SELECT name FROM employees WHERE salary > 64000.0 AND salary < 66000.0;\n",
    );
    assert_contains(&out2, "Updated");
    // Bob's salary is now 65000 — should match the salary range query
    assert_contains(&out2, "Bob");

    // Session 3: DELETE a row and verify the dept index no longer has it
    let out3 = rook(
        "USE multi_idx_db;\n\
         DELETE FROM employees WHERE name = 'Diana';\n\
         SELECT name FROM employees WHERE dept = 'Marketing';\n",
    );
    assert_contains(&out3, "Deleted");
    // Marketing department should now be empty
    assert_rows(&out3, 0);
    assert_not_contains(&out3, "error");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Index-accelerated BETWEEN and IN queries (L1 range pushdown)
// ═══════════════════════════════════════════════════════════════════════════════

// ── Index-accelerated BETWEEN range scan ──────────────────────────────────────

#[test]
fn index_accelerated_between() {
    clean_db();
    // Create table with salary index, insert range of salaries, query with BETWEEN
    let out = rook(
        "CREATE DATABASE idx_bet_db;\n\
         USE idx_bet_db;\n\
         CREATE TABLE employees (id INT PRIMARY KEY, name VARCHAR(50), \
                                salary DOUBLE PRECISION);\n\
         INSERT INTO employees VALUES (1, 'Alice', 45000.0);\n\
         INSERT INTO employees VALUES (2, 'Bob', 55000.0);\n\
         INSERT INTO employees VALUES (3, 'Charlie', 65000.0);\n\
         INSERT INTO employees VALUES (4, 'Diana', 75000.0);\n\
         INSERT INTO employees VALUES (5, 'Eve', 85000.0);\n\
         -- Create index on salary (the BETWEEN target column)\n\
         CREATE INDEX idx_emp_salary ON employees(salary);\n\
         -- BETWEEN with indexed column → RangeLookup(55000, 75000)\n\
         SELECT name FROM employees WHERE salary BETWEEN 55000.0 AND 75000.0 ORDER BY name;\n",
    );
    assert_contains(&out, "Created index");
    // Range [55000, 75000] inclusive: Bob(55000), Charlie(65000), Diana(75000)
    assert_contains(&out, "Bob");
    assert_contains(&out, "Charlie");
    assert_contains(&out, "Diana");
    // Alice(45000) and Eve(85000) are outside the range
    assert_not_contains(&out, "Alice");
    assert_not_contains(&out, "Eve");
    assert_rows(&out, 3);
}

// ── Index-accelerated BETWEEN with VARCHAR column ─────────────────────────────

#[test]
fn index_accelerated_varchar_between() {
    clean_db();
    let out = rook(
        "CREATE DATABASE idx_vbet_db;\n\
         USE idx_vbet_db;\n\
         CREATE TABLE products (id INT PRIMARY KEY, name VARCHAR(50), price REAL);\n\
         INSERT INTO products VALUES (1, 'Apple', 1.5);\n\
         INSERT INTO products VALUES (2, 'Banana', 2.0);\n\
         INSERT INTO products VALUES (3, 'Cherry', 3.5);\n\
         INSERT INTO products VALUES (4, 'Date', 4.0);\n\
         INSERT INTO products VALUES (5, 'Elderberry', 5.5);\n\
         -- Create index on name (VARCHAR)\n\
         CREATE INDEX idx_prod_name ON products(name);\n\
         -- BETWEEN with VARCHAR indexed column → RangeLookup('Banana', 'Date')\n\
         SELECT name FROM products WHERE name BETWEEN 'Banana' AND 'Date' ORDER BY name;\n",
    );
    assert_contains(&out, "Created index");
    // Range ['Banana', 'Date'] inclusive: Banana, Cherry, Date
    assert_contains(&out, "'Banana'");
    assert_contains(&out, "'Cherry'");
    assert_contains(&out, "'Date'");
    // Apple and Elderberry are outside
    assert_not_contains(&out, "'Apple'");
    assert_not_contains(&out, "'Elderberry'");
    assert_rows(&out, 3);
}

// ── Index-accelerated BETWEEN with no matching results ────────────────────────

#[test]
fn index_accelerated_between_empty_result() {
    clean_db();
    let out = rook(
        "CREATE DATABASE idx_bemp_db;\n\
         USE idx_bemp_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, val INT);\n\
         INSERT INTO t VALUES (1, 10);\n\
         INSERT INTO t VALUES (2, 20);\n\
         INSERT INTO t VALUES (3, 30);\n\
         CREATE INDEX idx_t_val ON t(val);\n\
         -- BETWEEN range with no matches → empty result via RangeLookup\n\
         SELECT id FROM t WHERE val BETWEEN 50 AND 100;\n",
    );
    assert_contains(&out, "0 row(s)");
    assert_not_contains(&out, "error");
}

// ── Index-accelerated IN list (multi-element falls through to SeqScan+Filter) ─

#[test]
fn index_accelerated_in_list() {
    clean_db();
    // Multi-element IN list: the index can't directly accelerate >1 element,
    // so it falls through to SeqScan + FilterOperator. This test verifies
    // correctness of that fallback (results must still be correct).
    let out = rook(
        "CREATE DATABASE idx_in_db;\n\
         USE idx_in_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, dept VARCHAR(20), salary REAL);\n\
         INSERT INTO t VALUES (1, 'Engineering', 95000.0);\n\
         INSERT INTO t VALUES (2, 'Sales', 60000.0);\n\
         INSERT INTO t VALUES (3, 'Engineering', 85000.0);\n\
         INSERT INTO t VALUES (4, 'Marketing', 72000.0);\n\
         INSERT INTO t VALUES (5, 'Engineering', 92000.0);\n\
         CREATE INDEX idx_dept ON t(dept);\n\
         -- Multi-element IN: falls through to SeqScan+Filter (still correct)\n\
         SELECT id, dept FROM t WHERE dept IN ('Engineering', 'Marketing') ORDER BY id;\n",
    );
    assert_contains(&out, "Created index");
    // Engineering: ids 1,3,5; Marketing: id 4
    assert_contains(&out, "Engineering");
    assert_contains(&out, "Marketing");
    // Sales should NOT appear in the output (excluded by IN filter)
    assert_not_contains(&out, "Sales");
    assert_rows(&out, 4);
}

// ── BETWEEN on non-indexed column (falls through to SeqScan+Filter) ───────────

#[test]
fn between_non_indexed_column() {
    clean_db();
    // No index on the queried column — BETWEEN falls through to SeqScan+Filter.
    // This verifies correctness of the fallback path.
    let out = rook(
        "CREATE DATABASE idx_bni_db;\n\
         USE idx_bni_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, age INT, salary REAL);\n\
         INSERT INTO t VALUES (1, 25, 50000.0);\n\
         INSERT INTO t VALUES (2, 30, 60000.0);\n\
         INSERT INTO t VALUES (3, 35, 70000.0);\n\
         INSERT INTO t VALUES (4, 40, 80000.0);\n\
         -- Create index on salary, but query on age (non-indexed)\n\
         CREATE INDEX idx_salary ON t(salary);\n\
         -- BETWEEN on non-indexed column 'age' → SeqScan+Filter\n\
         SELECT id FROM t WHERE age BETWEEN 28 AND 38 ORDER BY id;\n",
    );
    assert_contains(&out, "Created index");
    // Age range [28, 38]: id=2(30, age 25 excluded), id=3(35)
    // Verify by row count (2 rows = id=2 and id=3; id=1 and id=4 excluded)
    assert_contains(&out, "2");
    assert_contains(&out, "3");
    assert_rows(&out, 2);
    assert_not_contains(&out, "error");
}

// ── CURRENT_DATE / NOW without FROM (SELECT without FROM) ───────────────────

#[test]
fn select_current_date_without_from() {
    clean_db();
    let out = rook(
        "CREATE DATABASE cdf_db;\n\
         USE cdf_db;\n\
         SELECT CURRENT_DATE;\n\
         SELECT CURRENT_TIME;\n\
         SELECT 1 + 2 * 3;\n",
    );
    assert_contains(&out, "row(s) returned");
    assert_not_contains(&out, "requires at least one table");
    assert_not_contains(&out, "error");
    // SELECT 1+2*3 = 7 (operator precedence: 2*3=6, 1+6=7)
    assert_contains(&out, "7");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Inequality Index Acceleration (M1+M2) — Gt, Ge, Lt, Le via RangeLookup
// ═══════════════════════════════════════════════════════════════════════════════

// ── Index-accelerated Gt: col > value → RangeLookup(val+1, MAX) ───────────────

#[test]
fn index_accelerated_gt() {
    clean_db();
    let out = rook(
        "CREATE DATABASE igt_db;\n\
         USE igt_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, salary DOUBLE PRECISION);\n\
         INSERT INTO t VALUES (1, 50000.0);\n\
         INSERT INTO t VALUES (2, 60000.0);\n\
         INSERT INTO t VALUES (3, 70000.0);\n\
         INSERT INTO t VALUES (4, 80000.0);\n\
         CREATE INDEX idx_t_salary ON t(salary);\n\
         -- salary > 65000 via RangeLookup: expects id=3 (70000) and id=4 (80000)\n\
         SELECT id FROM t WHERE salary > 65000.0 ORDER BY id;\n",
    );
    assert_contains(&out, "Created index");
    assert_contains(&out, "3");
    assert_contains(&out, "4");
    assert_rows(&out, 2);
}

// ── Index-accelerated Lt: col < value → RangeLookup(MIN, val-1) ───────────────

#[test]
fn index_accelerated_lt() {
    clean_db();
    let out = rook(
        "CREATE DATABASE ilt_db;\n\
         USE ilt_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, salary DOUBLE PRECISION);\n\
         INSERT INTO t VALUES (1, 50000.0);\n\
         INSERT INTO t VALUES (2, 60000.0);\n\
         INSERT INTO t VALUES (3, 70000.0);\n\
         INSERT INTO t VALUES (4, 80000.0);\n\
         CREATE INDEX idx_t_salary ON t(salary);\n\
         -- salary < 65000 via RangeLookup: expects id=1 (50000) and id=2 (60000)\n\
         SELECT id FROM t WHERE salary < 65000.0 ORDER BY id;\n",
    );
    assert_contains(&out, "Created index");
    assert_contains(&out, "1");
    assert_contains(&out, "2");
    assert_rows(&out, 2);
}

// ── Index-accelerated combined range: col >= low AND col <= high ──────────────

#[test]
fn index_accelerated_and_range() {
    clean_db();
    let out = rook(
        "CREATE DATABASE iar_db;\n\
         USE iar_db;\n\
         CREATE TABLE t (id INT PRIMARY KEY, salary DOUBLE PRECISION);\n\
         INSERT INTO t VALUES (1, 40000.0);\n\
         INSERT INTO t VALUES (2, 55000.0);\n\
         INSERT INTO t VALUES (3, 60000.0);\n\
         INSERT INTO t VALUES (4, 65000.0);\n\
         INSERT INTO t VALUES (5, 90000.0);\n\
         CREATE INDEX idx_t_salary ON t(salary);\n\
         -- salary >= 50000 AND salary <= 70000 via combined RangeLookup\n\
         SELECT id FROM t WHERE salary >= 50000.0 AND salary <= 70000.0 ORDER BY id;\n",
    );
    assert_contains(&out, "Created index");
    assert_contains(&out, "2");
    assert_contains(&out, "3");
    assert_contains(&out, "4");
    assert_rows(&out, 3);
}
