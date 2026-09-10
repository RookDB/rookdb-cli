//! RookDB interactive SQL shell.
//!
//! The REPL parses each statement with `rook-parser` and dispatches on the
//! typed `rook_ast::QueryPlan`, so the compiler catches unhandled statement
//! types instead of failing on JSON field names at runtime.
//!
//! # Multi-line input
//!
//! Statements may span any number of lines and multiple statements may share
//! one line. Input accumulates in a buffer until a *statement terminator*
//! appears: a semicolon that is not inside single quotes, double quotes,
//! backticks or a line comment. Only then is the statement executed, which
//! makes this valid:
//!
//! ```text
//! > CREATE TABLE t (
//! ...   id INT,
//! ...   name VARCHAR(50)
//! ... );
//! ```
//!
//! `exit` / `quit` (typed alone on a line) leave the shell.

mod convert;
mod db;
mod handlers;

use rook_ast::*;
use rook_parser::parse_sql;
use std::io::{self, Write};

fn main() -> io::Result<()> {
    storage_manager::backend::executor::row_select::register_where_parser(rook_parser::parse_where_text);
    storage_manager::backend::cache::register_check_parser(rook_parser::parse_check_expr);

    println!("--------------------------------------");
    println!("Welcome to RookDB");
    println!("--------------------------------------\n");

    let mut catalog = db::initialize_catalog();
    let mut current_db: Option<String> = None;

    // Accumulates input across lines until a terminator completes it.
    let mut pending = String::new();

    loop {
        let prompt = if pending.is_empty() { "> " } else { "... " };
        print!("{}", prompt);
        io::stdout().flush()?;

        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            // EOF (piped input): execute whatever is left, then stop.
            if !pending.trim().is_empty() {
                execute(&pending, &mut catalog, &mut current_db);
            }
            break;
        }
        pending.push_str(&line);

        // `exit`/`quit` only count when typed alone on a line.
        let trimmed = line.trim();
        if pending.ends_with('\n') && trimmed.is_empty() && pending.trim().is_empty() {
            continue; // blank line — keep waiting for SQL
        }
        if pending.trim().eq_ignore_ascii_case("exit")
            || pending.trim().eq_ignore_ascii_case("quit")
        {
            println!("Exiting RookDB..!");
            break;
        }

        // Execute every complete statement currently buffered; keep the
        // trailing fragment (if any) for the next round.
        let buffer = std::mem::take(&mut pending);
        let (complete, remainder) = split_complete_statements(&buffer);
        pending = remainder;

        for stmt in complete {
            execute(&stmt, &mut catalog, &mut current_db);
            // Statement boundary: make the statement's writes durable before
            // prompting again (matches the pre-cache flush-on-drop contract).
            storage_manager::backend::cache::checkpoint();
        }
    }

    // EOF path may have executed a trailing statement.
    storage_manager::backend::cache::checkpoint();
    Ok(())
}

/// Execute one SQL statement string, printing results or errors.
///
/// Errors are printed rather than propagated so the REPL stays alive and
/// tests can assert on messages via stdout/stderr.
fn execute(
    sql: &str,
    catalog: &mut storage_manager::catalog::Catalog,
    current_db: &mut Option<String>,
) {
    let sql = sql.trim();
    if sql.is_empty() || sql.starts_with("--") {
        return;
    }

    match parse_sql(sql) {
        Ok(plan) => match plan {
            QueryPlan::ShowDatabases => handlers::dql::handle_show_databases(catalog, current_db),
            QueryPlan::DropDatabase(ref p) => handlers::ddl::handle_drop_database(catalog, current_db, p),
            QueryPlan::CreateDatabase(ref p) => handlers::ddl::handle_create_database(catalog, current_db, p),
            QueryPlan::UseDatabase(ref name) => handlers::ddl::handle_use_database(catalog, current_db, name),
            QueryPlan::ShowTables => handlers::dql::handle_show_tables(catalog, current_db),
            QueryPlan::CreateTable(ref p) => handlers::ddl::handle_create_table(catalog, current_db, p),
            QueryPlan::Insert(ref p) => handlers::dml::handle_insert(catalog, current_db, &plan, p),
            QueryPlan::Select(_) => handlers::dql::handle_select(catalog, current_db, &plan),
            QueryPlan::Update(ref p) => handlers::dml::handle_update(catalog, current_db, p),
            QueryPlan::DropTable(ref p) => handlers::ddl::handle_drop_table(catalog, current_db, p),
            QueryPlan::AlterTable(ref p) => handlers::ddl::handle_alter_table(catalog, current_db, p),
            QueryPlan::CreateView(ref p) => handlers::ddl::handle_create_view(catalog, current_db, p),
            QueryPlan::DropView(ref p) => handlers::ddl::handle_drop_view(catalog, current_db, p),
            QueryPlan::Truncate(ref p) => handlers::ddl::handle_truncate(catalog, current_db, p),
            QueryPlan::SetOperation(_) => handlers::dql::handle_set_operation(catalog, current_db, &plan),
            QueryPlan::CreateTableAsSelect(ref p) => handlers::ddl::handle_create_table_as_select(catalog, current_db, p),
            QueryPlan::DropIndex(ref p) => handlers::ddl::handle_drop_index(catalog, current_db, p),
            QueryPlan::CreateIndex(ref p) => handlers::ddl::handle_create_index(catalog, current_db, p),
            QueryPlan::Delete(ref p) => handlers::dml::handle_delete(catalog, current_db, p),
            QueryPlan::Vacuum(ref p) => handlers::ddl::handle_vacuum(catalog, current_db, p),
            QueryPlan::Unknown(ref msg) => {
                println!("Unsupported SQL statement: {}", msg);
                Ok(())
            }
        },
        Err(err) => {
            println!("Parse error: {}", err);
            Ok(())
        }
    }
    // Handler errors are surfaced without exiting so the REPL stays alive.
    .unwrap_or_else(|e| eprintln!("Error: {}", e));
}

/// Split accumulated input into complete statements plus the trailing
/// fragment that still awaits its terminator.
///
/// A statement ends at a semicolon that is not inside a quoted region.
/// Quote scanning understands `'...'` strings (with `''` escapes),
/// `"..."`/`` `...` `` identifiers and `--` line comments. A trailing
/// comment is stripped from each returned statement so stray `--` text can
/// never swallow an otherwise complete statement.
fn split_complete_statements(buffer: &str) -> (Vec<String>, String) {
    let mut statements = Vec::new();
    let mut current = String::new();

    let mut chars = buffer.chars().peekable();
    let mut quote: Option<char> = None;
    let mut in_line_comment = false;

    while let Some(c) = chars.next() {
        if in_line_comment {
            // Comments are dropped entirely so they can never hide a
            // statement that follows them on the same logical input.
            if c == '\n' {
                current.push('\n');
                in_line_comment = false;
            }
            continue;
        }

        match quote {
            Some(q) => {
                current.push(c);
                if c == q {
                    // Doubled quote (`''`) is an escape, stay inside the string.
                    if chars.peek() == Some(&q) {
                        current.push(chars.next().unwrap());
                    } else {
                        quote = None;
                    }
                }
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    current.push(c);
                }
                '-' if chars.peek() == Some(&'-') => {
                    in_line_comment = true;
                }
                ';' => {
                    statements.push(current.trim().to_string());
                    current.clear();
                }
                _ => current.push(c),
            },
        }
    }

    // Strip a trailing line comment from an unterminated fragment so the
    // prompt does not hang on "-- pending".
    let remainder = current.trim().to_string();
    (statements, remainder)
}
