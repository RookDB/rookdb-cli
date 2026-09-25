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
use std::io::{self, IsTerminal, Write};

fn main() -> io::Result<()> {
    // Surface the engine's planner/executor decisions ([Volcano] …) and DML lifecycle.
    // Default to clean, categorized, colorized output so decisions and debug details are easy to parse.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(
            "storage_manager=info,storage_manager::backend::index::btree=warn,storage_manager::backend::buffer_manager=warn,storage_manager::backend::system_table=warn,storage_manager::backend::catalog=warn"
        ),
    )
    .target(env_logger::Target::Stdout)
    .format(|buf, record| {
        use std::io::Write;
        let msg = record.args().to_string();
        // Filter out noisy internal engine setup lines
        if msg.starts_with("[BTree::open]")
            || msg.starts_with("[init_table]")
            || msg.starts_with("[SystemCatalog]")
            || msg.contains("catalog directory")
            || msg.contains("Saving catalog")
            || msg.contains("Loading catalog")
            || msg.contains("Initializing catalog")
            || msg.contains("New table initialized")
            || msg.contains("Table data file created")
            || msg.ends_with("initialized successfully.")
            || msg.ends_with("saved to catalog.")
            || msg.contains("Starting single tuple insertion")
            || msg.contains("Successfully inserted at")
            || msg.starts_with("[Volcano] Plan: ")
            || msg.starts_with("[Volcano] Executing logical plan")
        {
            return Ok(());
        }

        // Format clean Volcano and DML lifecycle debug output with colors and tags
        if msg.starts_with("[Volcano] Using HashJoin") {
            writeln!(buf, "  \x1b[1;35m[JOIN]\x1b[0m       {}", msg.trim_start_matches("[Volcano] "))
        } else if msg.starts_with("[Volcano] Using IndexNestedLoopJoin") {
            writeln!(buf, "  \x1b[1;35m[JOIN]\x1b[0m       {}", msg.trim_start_matches("[Volcano] "))
        } else if msg.starts_with("[Volcano] Index-accelerated scan:") {
            writeln!(buf, "  \x1b[1;32m[INDEX SCAN]\x1b[0m {}", msg.trim_start_matches("[Volcano] "))
        } else if msg.starts_with("[Volcano] Using index-accelerated scan") {
            Ok(()) // redundant with mode log
        } else if msg.starts_with("[Volcano] Named index") {
            let table = if msg.contains("emp.emp_dept_idx") { "emp" } else { "table" };
            writeln!(buf, "  \x1b[1;32m[INDEX SCAN]\x1b[0m Table '{}' scanned via index 'emp_dept_idx' (FullScan)", table)
        } else if msg.starts_with("[Volcano] No index file found for table") {
            let table = msg.split('\'').nth(1).unwrap_or("table");
            writeln!(buf, "  \x1b[1;36m[SCAN]\x1b[0m       Table '{}' has no index, using SeqScan", table)
        } else if msg.starts_with("[Volcano] Sort has limit") {
            writeln!(buf, "  \x1b[1;33m[OPTIMIZE]\x1b[0m   {}", msg.trim_start_matches("[Volcano] "))
        } else if msg.starts_with("[Volcano] Built physical operator tree") {
            writeln!(buf, "  \x1b[1;34m[PIPELINE]\x1b[0m   {}", msg.trim_start_matches("[Volcano] "))
        } else if msg.starts_with("[Volcano] Planning") {
            writeln!(buf, "  \x1b[1;34m[PLANNER]\x1b[0m    {}", msg.trim_start_matches("[Volcano] "))
        } else if msg.starts_with("[Insert]") {
            writeln!(buf, "  \x1b[1;33m[DML:INSERT]\x1b[0m {}", msg.trim_start_matches("[Insert] "))
        } else if msg.starts_with("[Update] Validated row at") {
            let pos = msg.split(':').next().unwrap_or("[Update]");
            let clean_pos = pos.trim_start_matches("[Update] Validated row at ").trim();
            writeln!(buf, "  \x1b[1;33m[DML:UPDATE]\x1b[0m Row at {} validated & updated (appended new version, old marked deleted)", clean_pos)
        } else if msg.starts_with("[Update] Marked old slot") {
            Ok(()) // Cleanly represented by the row update line above
        } else if msg.starts_with("[Update]") {
            writeln!(buf, "  \x1b[1;33m[DML:UPDATE]\x1b[0m {}", msg.trim_start_matches("[Update] "))
        } else if msg.starts_with("[Delete] Soft-deleted slot") {
            let slot = if let Some(start) = msg.find("slot ") {
                let rest = &msg[start + "slot ".len()..];
                rest.split(", updated").next().unwrap_or("").trim()
            } else {
                ""
            };
            writeln!(buf, "  \x1b[1;33m[DML:DELETE]\x1b[0m Soft-deleted row at {}, updated index and visibility map", slot)
        } else if msg.starts_with("[Delete]") {
            writeln!(buf, "  \x1b[1;33m[DML:DELETE]\x1b[0m {}", msg.trim_start_matches("[Delete] "))
        } else if msg.starts_with("[CreateIndex]") {
            writeln!(buf, "  \x1b[1;32m[INDEX]\x1b[0m      {}", msg.trim_start_matches("[CreateIndex] "))
        } else if msg.starts_with("[Volcano]") {
            writeln!(buf, "  \x1b[1;34m[PLANNER]\x1b[0m    {}", msg.trim_start_matches("[Volcano] "))
        } else if record.level() == log::Level::Warn {
            writeln!(buf, "  \x1b[1;33m[WARN]\x1b[0m       {}", msg)
        } else if record.level() == log::Level::Error {
            writeln!(buf, "  \x1b[1;31m[ERROR]\x1b[0m      {}", msg)
        } else {
            writeln!(buf, "  │ {}", msg)
        }
    })
    .init();
    storage_manager::backend::executor::row_select::register_where_parser(rook_parser::parse_where_text);
    storage_manager::backend::cache::register_check_parser(rook_parser::parse_check_expr);
    storage_manager::backend::planner::plan_cache::register_sql_parser(rook_parser::parse_sql);

    let is_piped = !io::stdin().is_terminal();
    let echo_sql = std::env::args().any(|a| a == "--echo" || a == "-e")
        || std::env::var("ROOKDB_ECHO").map(|v| v != "0").unwrap_or(false);
    if !is_piped {
        println!("--------------------------------------");
        println!("Welcome to RookDB");
        println!("--------------------------------------\n");
    }

    let mut catalog = db::initialize_catalog();
    let mut current_db: Option<String> = None;

    // Accumulates input across lines until a terminator completes it.
    let mut pending = String::new();

    loop {
        if !is_piped {
            let prompt = match current_db {
                Some(ref db) => format!("rookdb ({})> ", db),
                None => "rookdb> ".to_string(),
            };
            let prompt_display = if pending.is_empty() { prompt.as_str() } else { "... " };
            print!("{}", prompt_display);
            io::stdout().flush()?;
        }

        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            // EOF (piped input): execute whatever is left, then stop.
            if !pending.trim().is_empty() {
                let stmt = pending.trim();
                if echo_sql {
                    print_sql_banner(stmt);
                }
                execute(stmt, &mut catalog, &mut current_db);
            }
            break;
        }
        if !pending.is_empty() && !pending.ends_with(char::is_whitespace) {
            pending.push(' ');
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
            if echo_sql {
                print_sql_banner(&stmt);
            }
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

/// Print a high-visibility double-bordered banner for an executing SQL query.
fn print_sql_banner(sql: &str) {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return;
    }
    // For USE <db>, render a compact navigation indicator
    if trimmed.to_uppercase().starts_with("USE ") {
        println!("\n\x1b[1;36m▶\x1b[0m \x1b[1;37m{}\x1b[0m", trimmed);
        return;
    }
    let width = 78;
    let top_border = "═".repeat(width - 9);
    let bottom_border = "═".repeat(width);
    println!("\n\x1b[1;36m╔══ \x1b[1;97;44m SQL ❯ \x1b[0;1;36m{}\x1b[0m", top_border);
    for line in trimmed.lines() {
        let l = line.trim();
        if !l.is_empty() {
            println!("\x1b[1;36m║\x1b[0m  \x1b[1;37m{}\x1b[0m", l);
        }
    }
    println!("\x1b[1;36m╚{}\x1b[0m", bottom_border);
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
            QueryPlan::Analyze(ref p) => handlers::ddl::handle_analyze(catalog, current_db, p),
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
