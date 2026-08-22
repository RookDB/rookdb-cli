//! RookDB interactive SQL shell.
//!
//! This REPL parses each statement with `rook-parser` and dispatches on the
//! typed `rook_ast::QueryPlan` instead of matching on JSON field names. The
//! compiler now guarantees that every plan variant is handled explicitly.
//!
//! Execution still goes through the storage manager's direct API (catalog
//! calls plus the legacy SELECT/UPDATE/DELETE executors). Statement types
//! whose engine support arrives in later stages print a clear "not yet
//! supported" message, which makes the capability boundary of this stage
//! visible from the shell itself.

mod convert;
mod db;

use convert::{constant_to_raw_string, expr_to_sql_string, predicate_to_debug_string};
use rook_ast::{ExprNode, QueryPlan, SelectExpr};
use rook_parser::parse_sql;
use std::io::{self, Write};
use std::str::FromStr;
use storage_manager::catalog::{create_database, create_table, show_databases, show_tables};
use storage_manager::catalog::{Column, Constraints};
use storage_manager::insert_single_tuple;
use storage_manager::types::DataType;

fn main() -> io::Result<()> {
    println!("--------------------------------------");
    println!("Welcome to RookDB");
    println!("--------------------------------------\n");

    // Initialize storage manager catalog and track the selected database.
    let mut catalog = db::initialize_catalog();
    let mut current_db: Option<String> = None;

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
            println!("Exiting RookDB..!");
            break;
        }
        if input.is_empty() || input.starts_with("--") {
            continue;
        }

        match parse_sql(input) {
            Ok(plan) => {
                if let Err(e) = execute_plan(&plan, &mut catalog, &mut current_db) {
                    println!("Error: {}", e);
                }
            }
            Err(err) => println!("Parse error: {}", err),
        }
    }

    Ok(())
}

/// Route one parsed statement to its executor.
///
/// Variants handled here are fully executable; the remaining arms report when
/// a feature will land so the shell never fails silently on known SQL.
fn execute_plan(
    plan: &QueryPlan,
    catalog: &mut storage_manager::catalog::Catalog,
    current_db: &mut Option<String>,
) -> Result<(), String> {
    match plan {
        QueryPlan::ShowDatabases => {
            show_databases(catalog);
            Ok(())
        }
        QueryPlan::CreateDatabase(p) => {
            if create_database(catalog, &p.database) {
                println!("Database '{}' created successfully.", p.database);
            } else {
                println!("Database '{}' already exists.", p.database);
            }
            Ok(())
        }
        QueryPlan::UseDatabase(name) => {
            if catalog.databases.contains_key(name) {
                *current_db = Some(name.clone());
                println!("Database '{}' selected.", name);
                Ok(())
            } else {
                Err(format!("Database '{}' does not exist.", name))
            }
        }
        QueryPlan::ShowTables => {
            if let Some(db_name) = current_db {
                show_tables(catalog, db_name);
                Ok(())
            } else {
                Err("No database selected. Use 'USE <database>' first.".to_string())
            }
        }
        QueryPlan::CreateTable(p) => handle_create_table(catalog, current_db, p),
        QueryPlan::Insert(p) => handle_insert(catalog, current_db, p),
        QueryPlan::Select(sp) => handle_select(catalog, current_db, sp),
        QueryPlan::Update(up) => handle_update(catalog, current_db, up),
        QueryPlan::Delete(dp) => handle_delete(catalog, current_db, dp),

        // ── Parsed but not executable until later stages ──
        QueryPlan::DropTable(_) => unsupported("DROP TABLE", "the DDL stage"),
        QueryPlan::AlterTable(_) => unsupported("ALTER TABLE", "the DDL stage"),
        QueryPlan::Truncate(_) => unsupported("TRUNCATE TABLE", "the DDL stage"),
        QueryPlan::CreateView(_) => unsupported("CREATE VIEW", "the DDL stage"),
        QueryPlan::DropView(_) => unsupported("DROP VIEW", "the DDL stage"),
        QueryPlan::CreateTableAsSelect(_) => unsupported("CREATE TABLE AS SELECT", "the DDL stage"),
        QueryPlan::SetOperation(_) => unsupported("UNION/INTERSECT/EXCEPT", "the query-engine stage"),
        QueryPlan::CreateIndex(_) => unsupported("CREATE INDEX", "the indexing stage"),
        QueryPlan::DropIndex(_) => unsupported("DROP INDEX", "the indexing stage"),
        QueryPlan::DropDatabase(p) => unsupported(
            &format!("DROP DATABASE {}", p.database),
            "the DDL stage",
        ),
        QueryPlan::Unknown(msg) => {
            println!("Unsupported SQL statement: {}", msg);
            Ok(())
        }
    }
}

fn unsupported(statement: &str, stage: &str) -> Result<(), String> {
    Err(format!(
        "{} is recognised by the parser but not executable yet (planned for {}).",
        statement, stage
    ))
}

/// Create a table from typed column definitions.
fn handle_create_table(
    catalog: &mut storage_manager::catalog::Catalog,
    current_db: &Option<String>,
    plan: &rook_ast::CreateTablePlan,
) -> Result<(), String> {
    let db = require_db(current_db)?;

    let mut columns = Vec::new();
    for col in &plan.columns {
        let data_type = DataType::from_str(col.data_type.trim()).map_err(|err| {
            format!(
                "Unknown type '{}' for column '{}': {}",
                col.data_type, col.name, err
            )
        })?;

        columns.push(Column {
            name: col.name.clone(),
            data_type,
            nullable: !col.constraints.iter().any(|c| c.contains("NOT NULL")),
            constraints: map_constraints(&col.constraints),
        });
    }

    create_table(catalog, &db, &plan.table, columns);
    println!("Table '{}' created successfully.", plan.table);
    Ok(())
}

/// Translate parser constraint strings into the catalog's `Constraints`.
fn map_constraints(constraint_strings: &[String]) -> Constraints {
    let mut out = Constraints::default();
    for c in constraint_strings {
        let upper = c.to_uppercase();
        if upper.contains("NOT NULL") {
            out.not_null = true;
        } else if upper.contains("UNIQUE") || upper.contains("PRIMARY KEY") {
            out.unique = true;
        } else if upper.starts_with("DEFAULT") {
            out.default = Some(default_value_from_string(c));
        }
    }
    out
}

/// Extract the literal after the `DEFAULT` keyword into a `DataValue`.
fn default_value_from_string(constraint: &str) -> storage_manager::types::DataValue {
    let raw = constraint.trim_start_matches(|c: char| !c.is_whitespace()).trim();
    if let Ok(i) = raw.parse::<i64>() {
        return storage_manager::types::DataValue::Int(i as i32);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return storage_manager::types::DataValue::DoublePrecision(
            storage_manager::types::OrderedF64(f),
        );
    }
    if raw.eq_ignore_ascii_case("TRUE") {
        return storage_manager::types::DataValue::Bool(true);
    }
    if raw.eq_ignore_ascii_case("FALSE") {
        return storage_manager::types::DataValue::Bool(false);
    }
    storage_manager::types::DataValue::Varchar(raw.trim_matches('\'').to_string())
}

/// Insert literal rows through the storage manager's single-tuple API.
fn handle_insert(
    catalog: &mut storage_manager::catalog::Catalog,
    current_db: &Option<String>,
    plan: &rook_ast::InsertPlan,
) -> Result<(), String> {
    let db = require_db(current_db)?;
    let mut inserted = 0usize;

    for row in &plan.values {
        let mut owned: Vec<String> = Vec::with_capacity(row.len());
        for expr in row {
            match expr {
                ExprNode::Constant(cv) => owned.push(constant_to_raw_string(cv)),
                other => {
                    return Err(format!(
                        "Only constant values are supported in INSERT at this stage (got {:?}).",
                        other
                    ))
                }
            }
        }
        let values: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        match insert_single_tuple(catalog, &db, &plan.table, &values) {
            Ok(true) => inserted += 1,
            Ok(false) => return Err("Insert failed.".to_string()),
            Err(e) => return Err(format!("Insert error: {}", e)),
        }
    }
    println!("{} row(s) inserted.", inserted);
    Ok(())
}

/// Execute a simple single-table SELECT through the legacy scan/filter path.
fn handle_select(
    catalog: &storage_manager::catalog::Catalog,
    current_db: &Option<String>,
    plan: &rook_ast::SelectPlan,
) -> Result<(), String> {
    let db = require_db(current_db)?;

    // Only `SELECT * FROM t [WHERE ...]` is executable at this stage.
    let is_star = matches!(plan.projections.first(), Some(SelectExpr::Wildcard))
        && plan.projections.len() == 1;
    if !is_star || plan.from.len() != 1 || !plan.joins.is_empty() {
        return Err(
            "Projections, JOINs, GROUP BY and ORDER BY need the query engine (later stage); \
             this stage supports SELECT * FROM <table> [WHERE <cond>]."
                .to_string(),
        );
    }

    let where_clause = plan
        .selection
        .as_ref()
        .map(|pred| predicate_to_debug_string(pred));

    db::execute_select(catalog, &db, &plan.from[0].name, where_clause.as_deref())
}

/// Execute an UPDATE by reconstructing SET/WHERE text for the legacy executor.
fn handle_update(
    catalog: &storage_manager::catalog::Catalog,
    current_db: &Option<String>,
    plan: &rook_ast::UpdatePlan,
) -> Result<(), String> {
    let db = require_db(current_db)?;

    let set_clause = plan
        .assignments
        .iter()
        .map(|a| format!("{} = {}", a.column, expr_to_sql_string(&a.value)))
        .collect::<Vec<_>>()
        .join(", ");

    let where_clause = plan
        .selection
        .as_ref()
        .map(|pred| predicate_to_debug_string(pred));

    db::execute_update(catalog, &db, &plan.table, &set_clause, where_clause.as_deref())
}

/// Execute a DELETE through the legacy executor.
fn handle_delete(
    catalog: &storage_manager::catalog::Catalog,
    current_db: &Option<String>,
    plan: &rook_ast::DeletePlan,
) -> Result<(), String> {
    let db = require_db(current_db)?;

    let where_clause = plan
        .selection
        .as_ref()
        .map(|pred| predicate_to_debug_string(pred));

    db::execute_delete(catalog, &db, &plan.table, where_clause.as_deref())
}

/// Return the active database name or fail with the standard hint.
fn require_db(current_db: &Option<String>) -> Result<String, String> {
    current_db
        .clone()
        .ok_or_else(|| "No database selected. Use 'USE <database>' first.".to_string())
}
