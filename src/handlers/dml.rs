use std::io;

use rook_ast::*;
use storage_manager::catalog::Catalog;
use storage_manager::executor::physical::execute_plan_collect;
use storage_manager::executor::update_by_pointers;
use storage_manager::executor::delete_by_pointers;

use crate::handlers::helpers::expr_to_debug_string;

/// Handle INSERT (both VALUES and INSERT INTO ... SELECT)
pub fn handle_insert(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    plan: &QueryPlan,
    _ins: &InsertPlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };

    // Both INSERT forms route through the standard planner pipeline:
    //   INSERT ... SELECT  -> child plan produces rows
    //   INSERT ... VALUES  -> constant rows expanded by the logical planner
    match storage_manager::planner::plan_query(plan, catalog, &db) {
        Ok(logical_plan) => {
            // Collect silently — inserting rows should report a count, not
            // render every written tuple as a result table.
            match execute_plan_collect(&logical_plan, catalog, &db) {
                Ok(tuples) => {
                    let n = tuples.len();
                    if n == 1 {
                        println!("1 row inserted.\n");
                    } else {
                        println!("{} rows inserted.\n", n);
                    }
                }
                Err(e) => println!("Insert failed: {}", e),
            }
        }
        Err(e) => println!("Insert planning error: {}", e),
    }

    Ok(())
}

/// Handle UPDATE via Volcano-based row selection, with string-based fallback.
///
/// Uses the Volcano engine to identify matching rows via the WHERE clause
/// (benefiting from index-accelerated scans and full expression evaluation),
/// then applies SET assignments via `update_by_pointers`. Falls back to the
/// legacy string-based approach when Volcano returns 0 rows, ensuring
/// cross-session UPDATE works correctly.
pub fn handle_update(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    upd: &UpdatePlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };

    // Build a SELECT plan from the UPDATE's table + WHERE clause
    let select_plan = SelectPlan {
        ctes: vec![],
        projections: vec![SelectExpr::Wildcard],
        from: vec![TableRef { name: upd.table.clone(), alias: None }],
        joins: vec![],
        selection: upd.selection.clone(),
        group_by: vec![],
        having: None,
        order_by: vec![],
        limit: None,
        distinct: false,
    };
    let query_plan = QueryPlan::Select(select_plan);

    // Volcano is the single row-selection path: execute the SELECT plan and
    // rewrite the matching tuples in place via their heap locations. An
    // empty result simply means zero affected rows — no legacy fallback.
    let tuples = match storage_manager::planner::plan_query(&query_plan, catalog, &db)
        .map_err(|e| format!("Plan error: {}", e))
        .and_then(|logical_plan| execute_plan_collect(&logical_plan, catalog, &db).map_err(|e| e.to_string()))
    {
        Ok(tuples) => tuples,
        Err(e) => {
            println!("Update failed: {}", e);
            return Ok(());
        }
    };
    let pointers: Vec<(u32, u32)> = tuples.iter().filter_map(|t| {
        match (t.page_id, t.slot_id) {
            (Some(page), Some(slot)) => Some((page, slot)),
            _ => None,
        }
    }).collect();
    if pointers.len() != tuples.len() {
        println!("Update failed: engine returned rows without heap locations");
        return Ok(());
    }

    // Build parsed SET assignments from the AST
    let set_str = upd.assignments.iter()
        .map(|a| format!("{} = {}", a.column, expr_to_debug_string(&a.value)))
        .collect::<Vec<_>>().join(", ");
    let assignments = match storage_manager::executor::parse_set_clause(&set_str) {
        Some(a) => a,
        None => {
            println!("Could not parse SET clause.");
            return Ok(());
        }
    };

    match update_by_pointers(catalog, &db, &upd.table, &pointers, &assignments) {
        Ok(result) => {
            println!("\nUpdated {} row(s).", result.updated_count);
        }
        Err(e) => println!("Update failed: {}", e),
    }

    Ok(())
}

/// Handle DELETE via Volcano-based row selection, with string-based fallback.
///
/// Uses the Volcano engine to identify matching rows via the WHERE clause
/// (benefiting from index-accelerated scans and full expression evaluation),
/// then deletes them using `delete_by_pointers`. Falls back to the legacy
/// string-based approach when Volcano returns 0 rows, ensuring cross-session
/// DELETE works correctly.
pub fn handle_delete(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    del: &DeletePlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };

    // Build a SELECT plan from the DELETE's table + WHERE clause
    let select_plan = SelectPlan {
        ctes: vec![],
        projections: vec![SelectExpr::Wildcard],
        from: vec![TableRef { name: del.table.clone(), alias: None }],
        joins: vec![],
        selection: del.selection.clone(),
        group_by: vec![],
        having: None,
        order_by: vec![],
        limit: None,
        distinct: false,
    };
    let query_plan = QueryPlan::Select(select_plan);

    // Volcano is the single row-selection path (see handle_update).
    let tuples = match storage_manager::planner::plan_query(&query_plan, catalog, &db)
        .map_err(|e| format!("Plan error: {}", e))
        .and_then(|logical_plan| execute_plan_collect(&logical_plan, catalog, &db).map_err(|e| e.to_string()))
    {
        Ok(tuples) => tuples,
        Err(e) => {
            println!("Delete failed: {}", e);
            return Ok(());
        }
    };
    let pointers: Vec<(u32, u32)> = tuples.iter().filter_map(|t| {
        match (t.page_id, t.slot_id) {
            (Some(page), Some(slot)) => Some((page, slot)),
            _ => None,
        }
    }).collect();
    if pointers.len() != tuples.len() {
        println!("Delete failed: engine returned rows without heap locations");
        return Ok(());
    }

    match delete_by_pointers(catalog, &db, &del.table, &pointers) {
        Ok(result) => {
            println!("\nDeleted {} row(s).", result.deleted_count);
        }
        Err(e) => println!("Delete failed: {}", e),
    }

    Ok(())
}
