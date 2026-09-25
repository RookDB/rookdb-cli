use std::io;

use rook_ast::*;
use storage_manager::catalog::{Catalog, show_databases, show_tables};
use storage_manager::executor::physical::execute_plan;

/// Handle SELECT
pub fn handle_select(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    plan: &QueryPlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };

    match storage_manager::planner::plan_query(plan, catalog, &db) {
        Ok(logical_plan) => match execute_plan(&logical_plan, catalog, &db) {
            Ok(count) => {
                println!("{} row(s) returned.\n", count);
            }
            Err(e) => {
                println!("Execution error: {}", e);
            }
        },
        Err(e) => {
            println!("Planning error: {}", e);
        }
    }

    Ok(())
}

/// Handle SetOperation (UNION / INTERSECT / EXCEPT)
pub fn handle_set_operation(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    plan: &QueryPlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };

    match storage_manager::planner::plan_query(plan, catalog, &db) {
        Ok(logical_plan) => match execute_plan(&logical_plan, catalog, &db) {
            Ok(count) => {
                println!("{} row(s) returned.\n", count);
            }
            Err(e) => {
                println!("Execution error: {}", e);
            }
        },
        Err(e) => {
            println!("Planning error: {}", e);
        }
    }

    Ok(())
}

/// Handle SHOW DATABASES
pub fn handle_show_databases(
    catalog: &Catalog,
    _current_db: &mut Option<String>,
) -> io::Result<()> {
    show_databases(catalog);
    Ok(())
}

/// Handle SHOW TABLES
pub fn handle_show_tables(catalog: &Catalog, current_db: &mut Option<String>) -> io::Result<()> {
    if let Some(ref db_name) = *current_db {
        show_tables(catalog, db_name);
    } else {
        println!("No database selected. Use 'USE <database>' first.");
    }
    Ok(())
}
