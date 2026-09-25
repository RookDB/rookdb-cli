//! Shared helpers for CLI integration tests.
//!
//! [`Workspace`] creates an isolated directory per test and removes it when
//! the guard drops — including on panic — so test runs never litter the
//! crate directory with `database*` workspaces.

use std::path::PathBuf;

pub struct Workspace {
    path: PathBuf,
}

impl Workspace {
    /// Create `<crate>/database_<pid>_<name>` fresh.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn new(name: &str) -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "database_p{}_{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create workspace");
        Workspace { path }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl std::ops::Deref for Workspace {
    type Target = str;
    fn deref(&self) -> &str {
        self.path.to_str().expect("workspace path is utf-8")
    }
}

impl AsRef<std::path::Path> for Workspace {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

// ── Running the shell ────────────────────────────────────────────────────────

/// Spawn a fresh CLI session inside `ws` and feed it SQL lines.
/// Returns combined stdout + stderr.
pub fn run(ws: &Workspace, sql_lines: &[&str]) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_rookdb"))
        .current_dir(ws.path())
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

/// Run a single statement in its own session.
#[allow(dead_code)]
pub fn run_one(ws: &Workspace, sql: &str) -> String {
    run(ws, &[sql])
}

// ── Value-level assertions ───────────────────────────────────────────────────
//
// The engine renders results as box-drawing tables:
//
//     ┌────────┬────────┐
//     │ id: INT│ name   │      header cells: "name: TYPE"
//     ├────────┼────────┤
//     │ 1  │ 1 │ 'x'    │  data rows: first cell is the row number,
//     └────────┴────────┘      remaining cells are the values
//
// `parse_result_table` strips borders and the row-number column so tests can
// assert exact result sets — values, order and NULL rendering included.

/// One parsed result table.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl QueryTable {
    /// The values of one column (in row order).
    #[allow(dead_code)]
    pub fn column(&self, name: &str) -> Vec<String> {
        let idx = self
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("column '{}' not found in {:?}", name, self.columns));
        self.rows.iter().map(|r| r[idx].clone()).collect()
    }
}

/// Parse all result tables from a session's output.
/// Remove a leading REPL prompt ("> " / ">" / "...") plus whitespace.
/// Returns the line unchanged when no prompt is present.
fn strip_prompt(line: &str) -> &str {
    // Prompts STACK: every accumulated continuation line prints another
    // "... ", so a completed multi-line statement's first output can carry
    // several of them glued together ("... ... > ┌───┐").
    let mut t = line.trim_start();
    loop {
        t = t.trim_start();
        if let Some(rest) = t.strip_prefix("> ") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix('>') {
            t = rest.trim_start();
        } else if let Some(rest) = t.strip_prefix("...") {
            t = rest;
        } else {
            return t;
        }
    }
}

pub fn parse_tables(output: &str) -> Vec<QueryTable> {
    let mut tables: Vec<QueryTable> = Vec::new();
    let mut cur: Option<QueryTable> = None;

    for line in output.lines() {
        // The REPL prints its prompt ("​> " or "...") without a newline, so
        // the first output line of a statement shares the prompt's line.
        let line = strip_prompt(line);
        match line.chars().next() {
            Some('┌') => {
                if let Some(t) = cur.take() {
                    tables.push(t);
                }
                cur = Some(QueryTable {
                    columns: vec![],
                    rows: vec![],
                });
            }
            Some('│') => {
                let Some(t) = cur.as_mut() else {
                    panic!("result cell outside of any table: {:?}", line);
                };
                let cells: Vec<String> = line
                    .split('│')
                    .skip(1) // text before first border
                    .map(str::trim)
                    .map(String::from)
                    .collect();
                let cells = &cells[..cells.len().saturating_sub(1)]; // tail after last border

                if t.columns.is_empty() && !t.rows.is_empty() {
                    // second table started without its header captured — ignore
                    continue;
                }
                if t.rows.is_empty() && t.columns.is_empty() {
                    // header line: cells are "name: TYPE" (leading row# cell without ':' is skipped)
                    let header_cells = if cells.first().map_or(false, |c| !c.contains(':')) {
                        &cells[1..]
                    } else {
                        &cells[..]
                    };
                    for c in header_cells {
                        let name = c.rsplit_once(": ").map(|(n, _)| n).unwrap_or(c).to_string();
                        t.columns.push(name);
                    }
                } else {
                    // data row: first cell is the 1-based row number
                    t.rows.push(cells[1..].to_vec());
                }
            }
            Some('(') if cur.is_some() => {
                // "(N rows)" footer ends the table — flush it so the next
                // statement's header cannot be mistaken for stray data.
                if let Some(t) = cur.take() {
                    tables.push(t);
                }
            }
            _ => {}
        }
    }
    if let Some(t) = cur.take() {
        tables.push(t);
    }
    tables
}

/// Parse the LAST result table of a session output (empty when none).
pub fn parse_last_table(output: &str) -> QueryTable {
    parse_tables(output).pop().unwrap_or(QueryTable {
        columns: vec![],
        rows: vec![],
    })
}

fn fmt_rows(rows: &[Vec<String>]) -> String {
    rows.iter()
        .map(|r| format!("  {:?}", r))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Assert that running `pre` followed by `sql` produces exactly `expected`
/// rows — same values, same order. Cell strings must match the engine's
/// rendering: quote string literals (`'bolt'`), write `NULL` literally,
/// keep numbers/floats as their Display form. Values over ~22 characters
/// are truncated by the display layer with an ellipsis.
#[track_caller]
pub fn expect_rows(ws: &Workspace, pre: &[&str], sql: &str, expected: &[&[&str]]) {
    // `pre` + `sql` must share ONE session: database selection via USE does
    // not persist across processes, only the on-disk catalog/data does.
    let mut lines: Vec<&str> = pre.to_vec();
    lines.push(sql);
    let out = run(ws, &lines);
    let table = parse_last_table(&out);

    let exp: Vec<Vec<String>> = expected
        .iter()
        .map(|r| r.iter().map(|c| c.to_string()).collect())
        .collect();

    assert!(
        table.rows == exp,
        "query returned wrong values\n\nSQL: {}\n\nexpected:\n{}\n\ngot:\n{}\n\nraw output:\n{}",
        sql,
        fmt_rows(&exp),
        fmt_rows(&table.rows),
        out
    );
}
