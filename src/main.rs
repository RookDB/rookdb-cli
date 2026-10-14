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
//!
//! # Flags
//!
//! - `--echo` / `-e`: print each statement before its result.
//! - `--engine-log` / `-v`: also print the engine's debug log (otherwise
//!   only its warnings and errors). `RUST_LOG` overrides the filter.
//! - `--log-operations`: keep the per-statement JSON operation log under
//!   `database/logs/`.

use rook_parser::parse_sql;
use std::io::{self, IsTerminal, Write};
use storage_manager::backend::session::Session;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |names: &[&str]| args.iter().any(|a| names.contains(&a.as_str()));

    // The shell shows statement results and messages; of the engine's log,// only warnings and errors. `--engine-log` adds the engine's debug log
    // (planner notes, per-statement DML summaries); RUST_LOG overrides both.
    // Plan decisions are not read from the log: `EXPLAIN <query>` returns them.
    let default_filter = if flag(&["--engine-log", "-v"]) {
        "warn,storage_manager=debug,storage_manager::backend::index::btree=warn,storage_manager::backend::buffer_manager=warn,storage_manager::backend::system_table=warn,storage_manager::backend::catalog=warn"
    } else {
        "warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .target(env_logger::Target::Stdout)
        .format(|buf, record| {
            use std::io::Write;
            let tag = match record.level() {
                log::Level::Error => "[1;31m[ERROR][0m",
                log::Level::Warn => "[1;33m[WARN][0m ",
                log::Level::Info => "[INFO] ",
                log::Level::Debug => "[DEBUG]",
                log::Level::Trace => "[TRACE]",
            };
            writeln!(buf, "  {} {}", tag, record.args())
        })
        .init();
    storage_manager::backend::executor::row_select::register_where_parser(
        rook_parser::parse_where_text,
    );
    storage_manager::backend::cache::register_check_parser(rook_parser::parse_check_expr);
    storage_manager::backend::planner::plan_cache::register_sql_parser(rook_parser::parse_sql);

    let is_piped = !io::stdin().is_terminal();
    let echo_sql = flag(&["--echo", "-e"])
        || std::env::var("ROOKDB_ECHO")
            .map(|v| v != "0")
            .unwrap_or(false);
    if !is_piped {
        println!("--------------------------------------");
        println!("Welcome to RookDB");
        println!("--------------------------------------\n");
    }

    storage_manager::catalog::init_catalog();
    let catalog = storage_manager::catalog::load_catalog();
    // `--log-operations`: UPDATE, DELETE and compaction append a JSON line
    // per statement under database/logs/ (off by default).
    catalog
        .engine()
        .set_log_operations(flag(&["--log-operations"]));
    let mut session = Session::new(catalog);

    // Accumulates input across lines until a terminator completes it.
    let mut pending = String::new();

    loop {
        if !is_piped {
            let prompt = match session.current_db {
                Some(ref db) => format!("rookdb ({})> ", db),
                None => "rookdb> ".to_string(),
            };
            let prompt_display = if pending.is_empty() {
                prompt.as_str()
            } else {
                "... "
            };
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
                execute(stmt, &mut session);
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
            execute(&stmt, &mut session);
            // Statement boundary: write the statement's pages to their files.
            // The fsync is shared by statements arriving within the sync
            // interval, so a script does not pay one per row.
            storage_manager::backend::cache::checkpoint();
        }
    }

    // Everything still un-fsynced goes to disk before the process exits.
    storage_manager::backend::cache::checkpoint_durable();
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
    println!(
        "\n\x1b[1;36m╔══ \x1b[1;97;44m SQL ❯ \x1b[0;1;36m{}\x1b[0m",
        top_border
    );
    for line in trimmed.lines() {
        let l = line.trim();
        if !l.is_empty() {
            println!("\x1b[1;36m║\x1b[0m  \x1b[1;37m{}\x1b[0m", l);
        }
    }
    println!("\x1b[1;36m╚{}\x1b[0m", bottom_border);
}

/// Execute one SQL statement string, printing its results or error.
///
/// Errors are printed rather than propagated so the REPL stays alive and
/// tests can assert on messages via stdout.
fn execute(sql: &str, session: &mut Session) {
    let sql = sql.trim();
    if sql.is_empty() || sql.starts_with("--") {
        return;
    }
    let plan = match parse_sql(sql) {
        Ok(plan) => plan,
        Err(err) => {
            println!("Parse error: {}", err);
            return;
        }
    };
    match session.execute(plan) {
        Ok(result) => {
            if let Some(plan) = &result.plan {
                // EXPLAIN: the plan as an indented tree, not a table of
                // quoted, width-capped cells.
                for line in plan.lines() {
                    println!("{}", line);
                }
            } else if let Some((schema, rows)) = &result.rows {
                storage_manager::executor::physical::tuple::display_tuples(rows, schema);
            }
            for line in &result.messages {
                println!("{}", line);
            }
        }
        Err(e) => println!("{}", e),
    }
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
