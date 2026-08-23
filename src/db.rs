use std::io;
use std::path::Path;

use storage_manager::catalog::{Catalog, create_table, init_catalog, load_catalog};
use storage_manager::heap::HeapManager;
use storage_manager::types::{deserialize_nullable_row, serialize_nullable_typed_row, DataValue};
use storage_manager::insert_single_tuple;

pub fn initialize_catalog() -> Catalog {
    init_catalog();
    load_catalog()
}

fn select_plan_references_table(plan: &rook_ast::SelectPlan, table: &str) -> bool {
    // Check FROM
    for tr in &plan.from {
        if tr.name.eq_ignore_ascii_case(table) {
            return true;
        }
    }
    // Check JOINs
    for jc in &plan.joins {
        if jc.relation.name.eq_ignore_ascii_case(table) {
            return true;
        }
    }
    // Check CTEs
    for cte in &plan.ctes {
        if select_plan_references_table(&cte.query, table) {
            return true;
        }
        if let Some(rt) = &cte.recursive_term {
            if select_plan_references_table(rt, table) {
                return true;
            }
        }
    }
    // Check projections (ScalarSubquery)
    for proj in &plan.projections {
        match proj {
            rook_ast::SelectExpr::UnnamedExpr(expr) | rook_ast::SelectExpr::ExprWithAlias { expr, .. } => {
                if expr_references_table(expr, table) {
                    return true;
                }
            }
            _ => {}
        }
    }
    // Check selection (WHERE)
    if let Some(sel) = &plan.selection {
        if predicate_node_references_table(sel, table) {
            return true;
        }
    }
    // Check HAVING
    if let Some(hav) = &plan.having {
        if predicate_node_references_table(hav, table) {
            return true;
        }
    }
    false
}

fn expr_references_table(expr: &rook_ast::ExprNode, table: &str) -> bool {
    match expr {
        rook_ast::ExprNode::Column(_name) => {
            // Unqualified column name doesn't contain table name
            false
        }
        rook_ast::ExprNode::Compound(parts) => {
            if parts.len() >= 2 {
                parts[parts.len() - 2].eq_ignore_ascii_case(table)
            } else {
                false
            }
        }
        rook_ast::ExprNode::Binary { left, right, .. } => {
            expr_references_table(left, table) || expr_references_table(right, table)
        }
        rook_ast::ExprNode::Cast { expr: inner, .. } => expr_references_table(inner, table),
        rook_ast::ExprNode::Case { when_then_pairs, else_result } => {
            for (w, t_expr) in when_then_pairs {
                if expr_references_table(w, table) || expr_references_table(t_expr, table) {
                    return true;
                }
            }
            if let Some(el) = else_result {
                if expr_references_table(el, table) {
                    return true;
                }
            }
            false
        }
        rook_ast::ExprNode::Function { args, .. } => {
            for arg in args {
                match arg {
                    rook_ast::FunctionArg::Expr(inner) => {
                        if expr_references_table(inner, table) {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
            false
        }
        rook_ast::ExprNode::ScalarSubquery(info) => {
            select_plan_references_table(&info.select, table)
        }
        _ => false,
    }
}

fn predicate_node_references_table(pred: &rook_ast::PredicateNode, table: &str) -> bool {
    match pred {
        rook_ast::PredicateNode::BinaryOp { left, right, .. } => {
            predicate_node_references_table(left, table) || predicate_node_references_table(right, table)
        }
        rook_ast::PredicateNode::Not(inner) => predicate_node_references_table(inner, table),
        rook_ast::PredicateNode::Compare { left, right, .. } => {
            expr_references_table(left, table) || expr_references_table(right, table)
        }
        rook_ast::PredicateNode::IsNull(expr) | rook_ast::PredicateNode::IsNotNull(expr) => {
            expr_references_table(expr, table)
        }
        rook_ast::PredicateNode::Between { expr, low, high } => {
            expr_references_table(expr, table) || expr_references_table(low, table) || expr_references_table(high, table)
        }
        rook_ast::PredicateNode::InList { expr, list } => {
            if expr_references_table(expr, table) {
                return true;
            }
            for item in list {
                if expr_references_table(item, table) {
                    return true;
                }
            }
            false
        }
        rook_ast::PredicateNode::Like { expr, .. } => expr_references_table(expr, table),
        rook_ast::PredicateNode::Exists(info) => select_plan_references_table(&info.select, table),
        rook_ast::PredicateNode::InSubquery { expr, subquery, .. } => {
            expr_references_table(expr, table) || select_plan_references_table(&subquery.select, table)
        }
        rook_ast::PredicateNode::IsDistinctFrom { left, right } => {
            expr_references_table(left, table) || expr_references_table(right, table)
        }
        rook_ast::PredicateNode::IsBoolean { expr, .. } => expr_references_table(expr, table),
    }
}

/// Execute a DROP TABLE query — deletes the table file from disk and removes from catalog.
pub fn execute_drop_table(
    catalog: &mut Catalog,
    db: &str,
    table: &str,
    if_exists: bool,
    cascade: bool,
) -> io::Result<()> {
    // Validate database exists and table exists
    let table_exists = catalog
        .databases
        .get(db)
        .map(|d| d.tables.contains_key(table))
        .unwrap_or(false);

    if !catalog.databases.contains_key(db) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Database '{}' not found", db),
        ));
    }

    if !table_exists {
        if if_exists {
            println!("Table '{}' does not exist (IF EXISTS specified, skipping).", table);
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Table '{}' not found in database '{}'", table, db),
        ));
    }

    // Check referencing views
    let mut ref_views = Vec::new();
    if let Some(db_obj) = catalog.databases.get(db) {
        for (view_name, view_def) in &db_obj.views {
            if let Ok(select_plan) = serde_json::from_str::<rook_ast::SelectPlan>(&view_def.query_json) {
                if select_plan_references_table(&select_plan, table) {
                    ref_views.push(view_name.clone());
                }
            }
        }
    }

    if !ref_views.is_empty() && !cascade {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Cannot drop table '{}' because it is referenced by views: {}", table, ref_views.join(", ")),
        ));
    }

    // Check referencing foreign keys (RESTRICT check)
    let ref_fks = match storage_manager::backend::constraint::loaders::load_referencing_foreign_keys(db, table) {
        Ok(fks) => fks,
        Err(e) => {
            return Err(io::Error::new(io::ErrorKind::Other,
                format!("Failed to check referencing foreign keys: {}", e)));
        }
    };

    if !ref_fks.is_empty() && !cascade {
        let child_tbls: Vec<String> = ref_fks.iter().map(|fk| fk.0.clone()).collect();
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Cannot drop table '{}' because it is referenced by foreign keys in: {}", table, child_tbls.join(", ")),
        ));
    }

    // If cascade is true, automatically drop referencing views
    if cascade && !ref_views.is_empty() {
        if let Some(db_obj) = catalog.databases.get_mut(db) {
            for view_name in &ref_views {
                db_obj.views.remove(view_name);
                println!("Cascaded drop of view '{}'.", view_name);
            }
        }
    }

    // If cascade is true, automatically delete referencing foreign key constraints
    if cascade && !ref_fks.is_empty() {
        let count = storage_manager::backend::system_table::delete_referencing_foreign_keys(db, table)?;
        println!("Cascaded drop of {} referencing foreign key constraints.", count);
    }

    // Remove the table from the catalog
    if let Some(db_obj) = catalog.databases.get_mut(db) {
        db_obj.tables.remove(table);
    }

    // Remove heap file (.dat)
    let dat_path = format!("database/base/{}/{}.dat", db, table);
    let _ = std::fs::remove_file(&dat_path);

    // Remove FSM fork file (.dat.fsm)
    let fsm_path = format!("{}.fsm", dat_path);
    let _ = std::fs::remove_file(&fsm_path);

    // Remove all index files (*.idx and *.idx.meta) for this table
    remove_index_files_for_table(db, table, None);

    // Clean up system table metadata (constraints, columns, indexes)
    if let Err(e) = storage_manager::backend::system_table::delete_table_metadata(db, table) {
        eprintln!(
            "[DropTable] Warning: failed to clean up system table metadata for '{}.{}': {}",
            db, table, e
        );
    }

    // Save the updated catalog (triggers full system table rebuild)
    if let Err(e) = storage_manager::catalog::save_catalog(catalog) {
        eprintln!(
            "[DropTable] Warning: failed to save catalog after dropping table '{}.{}': {}",
            db, table, e
        );
    }

    println!("Dropped table '{}' from database '{}'.", table, db);
    Ok(())
}

/// Execute an ALTER TABLE query — modifies the table schema.
pub fn execute_alter_table(
    catalog: &mut Catalog,
    db: &str,
    alter: &rook_ast::AlterTablePlan,
) -> io::Result<()> {
    use rook_ast::AlterTableAction;

    // Check the database and table exist without holding long-lived borrows
    let table_name_is_valid = catalog
        .databases
        .get(db)
        .map(|d| d.tables.contains_key(&alter.table))
        .unwrap_or(false);

    if !catalog.databases.contains_key(db) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Database '{}' not found", db),
        ));
    }

    if !table_name_is_valid {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Table '{}' not found", alter.table),
        ));
    }

    // Clone the table for mutation
    let mut updated_table = catalog.databases[db].tables[&alter.table].clone();

    match &alter.action {
        AlterTableAction::RenameTable { new_name } => {
            if catalog.databases[db].tables.contains_key(new_name) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Table '{}' already exists", new_name),
                ));
            }
            let old_name = alter.table.clone();
            // Rename the heap file
            let old_dat = format!("database/base/{}/{}.dat", db, old_name);
            let new_dat = format!("database/base/{}/{}.dat", db, new_name);
            let _ = std::fs::rename(&old_dat, &new_dat);
            // Rename the FSM fork
            let old_fsm = format!("{}.fsm", old_dat);
            let new_fsm = format!("{}.fsm", new_dat);
            let _ = std::fs::rename(&old_fsm, &new_fsm);
            // Rename all index files for this table
            rename_index_files_for_table(db, &old_name, new_name);

            // Update catalog: remove old name, insert new
            if let Some(d) = catalog.databases.get_mut(db) {
                d.tables.remove(&old_name);
                d.tables.insert(new_name.clone(), updated_table);
            }
            if let Err(e) = storage_manager::catalog::save_catalog(catalog) {
                eprintln!(
                    "[AlterTable] Warning: failed to save catalog after renaming table '{}' → '{}': {}",
                    old_name, new_name, e
                );
            }
            println!("Renamed table '{}' to '{}'.", old_name, new_name);
            return Ok(());
        }
        AlterTableAction::AddColumn { column_def } => {
            let col = storage_manager::catalog::Column::new(
                column_def.name.clone(),
                column_def.data_type.parse().map_err(|e: String| {
                    io::Error::new(io::ErrorKind::InvalidInput, format!("Invalid data type: {}", e))
                })?,
            );
            // Check for duplicate column name
            if updated_table.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&column_def.name)) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Column '{}' already exists in table '{}'", column_def.name, alter.table),
                ));
            }
            // Capture old schema BEFORE pushing the new column (needed for backfill)
            let old_schema: Vec<storage_manager::types::DataType> = updated_table.columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect();

            updated_table.columns.push(col);

            // Backfill: migrate existing heap rows to include NULL for the new column.
            // This must happen before any subsequent scan/deserialization operation
            // (e.g. CREATE INDEX) that would fail on schema-mismatched rows.
            let dat_path = format!("database/base/{}/{}.dat", db, alter.table);
            if Path::new(&dat_path).exists() {
                let new_schema: Vec<storage_manager::types::DataType> = updated_table.columns
                    .iter()
                    .map(|c| c.data_type.clone())
                    .collect();

                // Open the old heap and scan all rows
                let old_heap = HeapManager::open(std::path::PathBuf::from(&dat_path))
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("Failed to open heap for backfill: {}", e)))?;

                let mut migrated_rows: Vec<Vec<u8>> = Vec::new();
                for result in old_heap.scan() {
                    let (_page_id, _slot_id, row_bytes) = result?;
                    // Deserialize with OLD schema (before ADD COLUMN)
                    let old_values = deserialize_nullable_row(&old_schema, &row_bytes)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                            format!("Failed to deserialize row during backfill: {}", e)))?;
                    // Append NULL for the new column
                    let mut new_values: Vec<Option<DataValue>> = old_values;
                    new_values.push(None);
                    // Re-serialize with NEW schema (includes the added column)
                    let new_row_bytes = serialize_nullable_typed_row(&new_schema, &new_values)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                            format!("Failed to serialize row during backfill: {}", e)))?;
                    migrated_rows.push(new_row_bytes);
                }
                // Drop the old heap (closes file handles)
                drop(old_heap);

                // Write to a temp file first for atomicity: if the backfill fails,
                // the original heap file remains intact.
                let tmp_path = format!("{}.backfill", dat_path);
                let tmp_fsm_path = format!("{}.fsm", tmp_path);

                // Create a fresh heap at the temp path
                {
                    let mut tmp_heap = HeapManager::create(std::path::PathBuf::from(&tmp_path))
                        .map_err(|e| io::Error::new(io::ErrorKind::Other,
                            format!("Failed to create temp heap for backfill: {}", e)))?;
                    for row in &migrated_rows {
                        tmp_heap.insert_tuple(row)?;
                    }
                    tmp_heap.flush()?;
                }

                // Atomic swap: remove old files, rename temp files into place
                let fsm_path = format!("{}.fsm", dat_path);
                std::fs::remove_file(&dat_path)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("Failed to remove old heap during backfill: {}", e)))?;
                let _ = std::fs::remove_file(&fsm_path);

                std::fs::rename(&tmp_path, &dat_path)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("Failed to rename backfill heap: {}", e)))?;
                let _ = std::fs::rename(&tmp_fsm_path, &fsm_path);

                // Remove stale index files — they reference old heap page/slot locations
                remove_index_files_for_table(db, &alter.table, None);
            }
        }
        AlterTableAction::DropColumn { column } => {
            let pos = updated_table
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(column))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Column '{}' not found in table '{}'", column, alter.table),
                    )
                })?;

            // Capture OLD schema BEFORE removing the column (needed to deserialize
            // existing rows from disk).
            let old_schema: Vec<storage_manager::types::DataType> = updated_table.columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect();

            updated_table.columns.remove(pos);

            // Backfill: rewrite every heap tuple so its serialised column count
            // matches the new schema.  Without this step, SELECT * fails with
            // "Header column count N does not match schema length M".
            let dat_path = format!("database/base/{}/{}.dat", db, alter.table);
            if Path::new(&dat_path).exists() {
                let new_schema: Vec<storage_manager::types::DataType> = updated_table.columns
                    .iter()
                    .map(|c| c.data_type.clone())
                    .collect();

                let old_heap = HeapManager::open(std::path::PathBuf::from(&dat_path))
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("Failed to open heap for drop-column backfill: {}", e)))?;

                let mut migrated_rows: Vec<Vec<u8>> = Vec::new();
                for result in old_heap.scan() {
                    let (_page_id, _slot_id, row_bytes) = result?;
                    // Deserialize with OLD schema (includes the column being dropped)
                    let old_values = deserialize_nullable_row(&old_schema, &row_bytes)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                            format!("Failed to deserialize row during drop-column backfill: {}", e)))?;
                    // Remove the dropped column's value
                    let mut new_values: Vec<Option<DataValue>> = old_values;
                    new_values.remove(pos);
                    // Re-serialize with NEW schema (without the dropped column)
                    let new_row_bytes = serialize_nullable_typed_row(&new_schema, &new_values)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
                            format!("Failed to serialize row during drop-column backfill: {}", e)))?;
                    migrated_rows.push(new_row_bytes);
                }
                drop(old_heap);

                // Write to a temp file for atomic swap
                let tmp_path = format!("{}.backfill", dat_path);
                let tmp_fsm_path = format!("{}.fsm", tmp_path);

                {
                    let mut tmp_heap = HeapManager::create(std::path::PathBuf::from(&tmp_path))
                        .map_err(|e| io::Error::new(io::ErrorKind::Other,
                            format!("Failed to create temp heap for drop-column backfill: {}", e)))?;
                    for row in &migrated_rows {
                        tmp_heap.insert_tuple(row)?;
                    }
                    tmp_heap.flush()?;
                }

                // Atomic swap
                let fsm_path = format!("{}.fsm", dat_path);
                std::fs::remove_file(&dat_path)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("Failed to remove old heap during drop-column backfill: {}", e)))?;
                let _ = std::fs::remove_file(&fsm_path);

                std::fs::rename(&tmp_path, &dat_path)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("Failed to rename backfill heap: {}", e)))?;
                let _ = std::fs::rename(&tmp_fsm_path, &fsm_path);

                // Remove stale index files
                remove_index_files_for_table(db, &alter.table, None);
            }
        }
        AlterTableAction::RenameColumn { old_name, new_name } => {
            let col = updated_table
                .columns
                .iter_mut()
                .find(|c| c.name.eq_ignore_ascii_case(old_name))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Column '{}' not found in table '{}'", old_name, alter.table),
                    )
                })?;
            col.name = new_name.clone();
        }
        AlterTableAction::SetDefault { column, default_expr } => {
            let col = updated_table
                .columns
                .iter_mut()
                .find(|c| c.name.eq_ignore_ascii_case(column))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Column '{}' not found in table '{}'", column, alter.table),
                    )
                })?;
            // Parse the default value string into a DataValue
            let default_dv = storage_manager::executor::create_index::parse_string_to_value(
                &col.data_type, default_expr,
            ).ok();
            col.constraints.default = default_dv;
        }
        AlterTableAction::DropDefault { column } => {
            let col = updated_table
                .columns
                .iter_mut()
                .find(|c| c.name.eq_ignore_ascii_case(column))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Column '{}' not found in table '{}'", column, alter.table),
                    )
                })?;
            col.constraints.default = None;
        }

        AlterTableAction::SetNotNull { column } => {
            let col_idx = updated_table
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(column))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Column '{}' not found in table '{}'", column, alter.table),
                    )
                })?;

            // Scan the heap file to validate there are no existing NULL values.
            // Uses an O(1) null-bitmap check per tuple (no full deserialisation).
            let dat_path = format!("database/base/{}/{}.dat", db, alter.table);
            if Path::new(&dat_path).exists() {
                let heap = HeapManager::open(std::path::PathBuf::from(&dat_path))
                    .map_err(|e| io::Error::new(io::ErrorKind::Other,
                        format!("Failed to open heap for SET NOT NULL validation: {}", e)))?;

                // Fast-path: if the file size is only the header page (8 KB),
                // there are no data pages and thus no rows to validate.
                let is_empty = match std::fs::metadata(&dat_path) {
                    Ok(meta) => meta.len() <= 8192,
                    Err(_) => false,
                };

                if !is_empty {
                    for result in heap.scan() {
                        let (_page_id, _slot_id, row_bytes) = result?;
                        // Lightweight null-bitmap check — avoids full deserialisation
                        if storage_manager::types::null_bitmap::is_column_null_in_row(&row_bytes, col_idx)
                            .unwrap_or(false)
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("Table '{}' contains NULL values in column '{}'; cannot alter to NOT NULL", alter.table, column)
                            ));
                        }
                    }
                }
            }

            let col = &mut updated_table.columns[col_idx];
            col.nullable = false;
            col.constraints.not_null = true;
        }

        AlterTableAction::DropNotNull { column } => {
            let col = updated_table
                .columns
                .iter_mut()
                .find(|c| c.name.eq_ignore_ascii_case(column))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Column '{}' not found in table '{}'", column, alter.table),
                    )
                })?;
            col.nullable = true;
            col.constraints.not_null = false;
        }
    }

    // Update sys_constraints metadata for column operations
    match &alter.action {
        AlterTableAction::DropColumn { column } => {
            if let Err(e) = storage_manager::backend::system_table::delete_column_constraints(
                db, &alter.table, column,
            ) {
                eprintln!(
                    "[AlterTable] Warning: failed to clean up constraint metadata for column '{}': {}",
                    column, e
                );
            }
        }
        AlterTableAction::RenameColumn { old_name, new_name } => {
            if let Err(e) = storage_manager::backend::system_table::rename_column_in_constraints(
                db, &alter.table, old_name, new_name,
            ) {
                eprintln!(
                    "[AlterTable] Warning: failed to update constraint metadata for column rename '{}' → '{}': {}",
                    old_name, new_name, e
                );
            }
        }
        _ => {}
    }

    // Update catalog
    if let Some(d) = catalog.databases.get_mut(db) {
        d.tables.insert(alter.table.clone(), updated_table);
    }
    if let Err(e) = storage_manager::catalog::save_catalog(catalog) {
        eprintln!(
            "[AlterTable] Warning: failed to save catalog after altering table '{}': {}",
            alter.table, e
        );
    }

    println!("Altered table '{}': {:?}.", alter.table, alter.action);
    Ok(())
}

/// Execute a TRUNCATE TABLE query — removes all data from a table.
pub fn execute_truncate(
    catalog: &Catalog,
    db: &str,
    table: &str,
) -> io::Result<()> {
    // Validate database and table exist
    let db_obj = catalog
        .databases
        .get(db)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("Database '{}' not found", db)))?;

    if !db_obj.tables.contains_key(table) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Table '{}' not found in database '{}'", table, db),
        ));
    }

    // Delete the heap file and recreate it (empty)
    let dat_path = format!("database/base/{}/{}.dat", db, table);

    // Recreate the table file using HeapManager
    let path = std::path::Path::new(&dat_path);
    if path.exists() {
        std::fs::remove_file(path).map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Failed to remove table file: {}", e))
        })?;
    }
    // Remove FSM fork
    let fsm_path = format!("{}.fsm", dat_path);
    let _ = std::fs::remove_file(&fsm_path);

    // Create fresh heap file
    let mut hm = storage_manager::heap::HeapManager::create(std::path::PathBuf::from(&dat_path))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to create table file: {}", e)))?;
    hm.flush().map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to flush table file: {}", e)))?;

    // Remove index files (they're stale after truncation)
    remove_index_files_for_table(db, table, None);

    // NOTE: sys_indexes entries are intentionally NOT cleaned up here.
    // delete_table_metadata() would also delete sys_columns and sys_constraints
    // (designed for DROP TABLE), which would corrupt the table schema.
    // The stale sys_indexes entries are cosmetic — the .idx files are gone,
    // and a full catalog save will rebuild them correctly.

    println!("Truncated table '{}' (0 rows).", table);
    Ok(())
}

/// Execute a CREATE VIEW query — stores the view definition as serialised JSON
/// in the catalog so it can be expanded during query planning.
pub fn execute_create_view(
    catalog: &mut Catalog,
    db: &str,
    view: &rook_ast::CreateViewPlan,
) -> io::Result<()> {
    // Validate database exists
    if !catalog.databases.contains_key(db) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Database '{}' not found", db),
        ));
    }

    // Serialise the view's SELECT plan to JSON for storage
    let query_json = serde_json::to_string(&view.query)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let view_def = storage_manager::catalog::types::ViewDef {
        query_json,
    };

    if let Some(db_obj) = catalog.databases.get_mut(db) {
        if db_obj.tables.contains_key(&view.name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("A table named '{}' already exists", view.name),
            ));
        }
        if db_obj.views.contains_key(&view.name) && !view.or_replace {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("View '{}' already exists (use OR REPLACE to modify)", view.name),
            ));
        }
        db_obj.views.insert(view.name.clone(), view_def);
    }

    // Persist catalog changes
    storage_manager::catalog::save_catalog(catalog)?;

    println!("View '{}' created successfully.", view.name);
    Ok(())
}

/// Execute a CREATE TABLE ... AS SELECT query.
///
/// 1. Plan and execute the SELECT query via the Volcano engine
/// 2. Infer the output column types from the SELECT result
/// 3. Create the new table with those column types
/// 4. Insert all SELECT result rows into the new table
pub fn execute_create_table_as_select(
    catalog: &mut Catalog,
    db: &str,
    ctas: &rook_ast::CreateTableAsSelectPlan,
) -> io::Result<()> {
    // Validate database exists
    if !catalog.databases.contains_key(db) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Database '{}' not found", db),
        ));
    }

    // Check table doesn't already exist
    if catalog.databases[db].tables.contains_key(&ctas.table) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("Table '{}' already exists", ctas.table),
        ));
    }

    // Plan and execute the source SELECT query
    let query_plan = rook_ast::QueryPlan::Select(*ctas.query.clone());
    let logical_plan = match storage_manager::planner::plan_query(&query_plan, catalog, db) {
        Ok(plan) => plan,
        Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidInput,
            format!("Failed to plan SELECT query: {}", e))),
    };

    let tuples = match storage_manager::executor::physical::engine::execute_plan_collect(
        &logical_plan, catalog, db,
    ) {
        Ok(rows) => rows,
        Err(e) => return Err(io::Error::new(io::ErrorKind::Other,
            format!("Failed to execute SELECT query: {}", e))),
    };

    if tuples.is_empty() {
        // No rows — infer column types from the plan's output schema
        let schema = logical_plan_schema(&logical_plan);
        let columns: Vec<storage_manager::catalog::Column> = schema.into_iter().map(|(name, dt_str)| {
            let dt = dt_str.parse::<storage_manager::types::DataType>()
                .unwrap_or(storage_manager::types::DataType::Varchar(255));
            storage_manager::catalog::Column {
                name,
                data_type: dt,
                nullable: true,
                constraints: storage_manager::catalog::types::Constraints::default(),
            }
        }).collect();

        if columns.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "Cannot determine column types for CREATE TABLE AS SELECT".to_string()));
        }

        // Create the table with inferred column types
        create_table(catalog, db, &ctas.table, columns);
        println!("Table '{}' created successfully (0 rows).", ctas.table);
        return Ok(());
    }

    // Infer column types from the first tuple's schema
    let first_tuple = &tuples[0];
    let columns: Vec<storage_manager::catalog::Column> = first_tuple.column_info.iter().map(|ci| {
        storage_manager::catalog::Column {
            name: ci.name.clone(),
            data_type: ci.data_type.clone(),
            nullable: true,
            constraints: storage_manager::catalog::types::Constraints::default(),
        }
    }).collect();

    // Create the table
    create_table(catalog, db, &ctas.table, columns);

    // Insert all rows from the SELECT result
    let mut inserted = 0usize;
    for tuple in &tuples {
        let value_strs: Vec<String> = tuple.values.iter().map(|v| match v {
            Some(dv) => format!("{}", dv),
            None => "NULL".to_string(),
        }).collect();
        let value_refs: Vec<&str> = value_strs.iter().map(|s| s.as_str()).collect();
        match insert_single_tuple(catalog, db, &ctas.table, &value_refs) {
            Ok(true) => inserted += 1,
            Ok(false) => println!("Insert failed for one row."),
            Err(e) => println!("Insert error: {}", e),
        }
    }

    println!("Table '{}' created successfully with {} row(s).", ctas.table, inserted);
    Ok(())
}

/// Extract the output column names and types from a LogicalPlan.
fn logical_plan_schema(plan: &rook_ast::logical::LogicalPlan) -> Vec<(String, String)> {
    use rook_ast::logical::*;
    match plan {
        LogicalPlan::Project(p) => {
            p.expressions.iter().map(|ne| {
                (ne.name.clone(), "VARCHAR(255)".to_string())
            }).collect()
        }
        LogicalPlan::TableScan(t) => {
            t.schema.columns.iter().map(|c| {
                (c.name.clone(), format!("{}", c.data_type))
            }).collect()
        }
        LogicalPlan::Filter(f) => logical_plan_schema(&f.child),
        LogicalPlan::Sort(s) => logical_plan_schema(&s.child),
        LogicalPlan::Limit(l) => logical_plan_schema(&l.child),
        LogicalPlan::Distinct(d) => logical_plan_schema(&d.child),
        LogicalPlan::Aggregate(a) => logical_plan_schema(&a.child),
        LogicalPlan::Join(j) => {
            let mut left = logical_plan_schema(&j.left);
            let right = logical_plan_schema(&j.right);
            left.extend(right);
            left
        }
        LogicalPlan::SetOp(s) => logical_plan_schema(&s.left),
        LogicalPlan::Subquery(sq) => logical_plan_schema(&sq.subquery),
        LogicalPlan::Cte(c) => logical_plan_schema(&c.outer),
        LogicalPlan::RecursiveCte(rc) => logical_plan_schema(&rc.outer),
        LogicalPlan::CteScan(cs) => {
            cs.schema.columns.iter().map(|c| {
                (c.name.clone(), format!("{}", c.data_type))
            }).collect()
        }
        LogicalPlan::Insert(inp) => logical_plan_schema(&inp.child),
    }
}

/// Execute a DROP VIEW query — removes the view definition from the catalog.
pub fn execute_drop_view(
    catalog: &mut Catalog,
    db: &str,
    view_name: &str,
    if_exists: bool,
) -> io::Result<()> {
    if !catalog.databases.contains_key(db) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Database '{}' not found", db),
        ));
    }

    let exists = catalog.databases.get(db)
        .map(|d| d.views.contains_key(view_name))
        .unwrap_or(false);

    if !exists {
        if if_exists {
            println!("View '{}' does not exist (IF EXISTS specified, skipping).", view_name);
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("View '{}' not found in database '{}'", view_name, db),
        ));
    }

    if let Some(db_obj) = catalog.databases.get_mut(db) {
        db_obj.views.remove(view_name);
    }

    // Persist catalog changes
    storage_manager::catalog::save_catalog(catalog)?;

    println!("View '{}' dropped.", view_name);
    Ok(())
}

/// Remove index files (*.idx and *.idx.meta) for a given table.
///
/// When `index_name` is `Some(name)`, removes only the specific named index file
/// (`{table}.{name}.idx` and `{table}.{name}.idx.meta`).
///
/// When `index_name` is `None`, removes ALL index files for the table:
/// - Legacy `{table}.idx` and `{table}.idx.meta`
/// - All named `{table}.{any_name}.idx` and `{table}.{any_name}.idx.meta`
pub fn remove_index_files_for_table(db: &str, table: &str, index_name: Option<&str>) {
    let base_dir = format!("database/base/{}", db);
    let base_path = std::path::Path::new(&base_dir);

    if !base_path.exists() {
        return;
    }

    if let Some(name) = index_name {
        // Remove just this specific named index
        let idx_path = format!("database/base/{}/{}.{}.idx", db, table, name);
        let _ = std::fs::remove_file(&idx_path);
        let meta_path = format!("{}.meta", idx_path);
        let _ = std::fs::remove_file(&meta_path);
    } else {
        // Remove legacy single-index file
        let legacy_idx = format!("database/base/{}/{}.idx", db, table);
        let _ = std::fs::remove_file(&legacy_idx);
        let legacy_meta = format!("{}.meta", legacy_idx);
        let _ = std::fs::remove_file(&legacy_meta);

        // Remove named index files: {table}.{index_name}.idx and .meta
        if let Ok(entries) = std::fs::read_dir(base_path) {
            let prefix = format!("{}.", table);
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                // Match {table}.{anything}.idx or {table}.{anything}.idx.meta
                if fname.starts_with(&prefix) && (fname.ends_with(".idx") || fname.ends_with(".idx.meta")) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Rename all index files for a table from `old_name` to `new_name`.
pub fn rename_index_files_for_table(db: &str, old_name: &str, new_name: &str) {
    let base_dir = format!("database/base/{}", db);
    let base_path = std::path::Path::new(&base_dir);

    if !base_path.exists() {
        return;
    }

    // Rename legacy single-index file
    let old_legacy = format!("database/base/{}/{}.idx", db, old_name);
    let new_legacy = format!("database/base/{}/{}.idx", db, new_name);
    let _ = std::fs::rename(&old_legacy, &new_legacy);
    let old_legacy_meta = format!("{}.meta", old_legacy);
    let new_legacy_meta = format!("{}.meta", new_legacy);
    let _ = std::fs::rename(&old_legacy_meta, &new_legacy_meta);

    // Rename named index files: {old_name}.{index_name}.idx → {new_name}.{index_name}.idx
    if let Ok(entries) = std::fs::read_dir(base_path) {
        let prefix = format!("{}.", old_name);
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with(&prefix) && (fname.ends_with(".idx") || fname.ends_with(".idx.meta")) {
                // Extract the suffix after {old_name}.
                let suffix = fname.strip_prefix(&prefix).unwrap_or(&fname);
                let new_fname = format!("database/base/{}/{}.{}", db, new_name, suffix);
                let _ = std::fs::rename(entry.path(), &new_fname);
            }
        }
    }
}

/// Execute a DROP INDEX query — deletes the index file and metadata.
pub fn execute_drop_index(
    catalog: &Catalog,
    db: &str,
    index_name: &str,
    table_name: &str,
    if_exists: bool,
) -> io::Result<()> {
    // Validate database exists
    if !catalog.databases.contains_key(db) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Database '{}' not found", db),
        ));
    }

    // If we have a table name, try to drop by table
    if !table_name.is_empty() {
        // Try named index file first: {table}.{index_name}.idx
        let named_idx_path = format!("database/base/{}/{}.{}.idx", db, table_name, index_name);
        // Also try legacy single-index file: {table}.idx
        let legacy_idx_path = format!("database/base/{}/{}.idx", db, table_name);
        let legacy_meta_path = format!("{}.meta", legacy_idx_path);

        // Try named path first
        if std::path::Path::new(&named_idx_path).exists() {
            // Use the shared helper to remove the named index files
            remove_index_files_for_table(db, table_name, Some(index_name));
        } else if std::path::Path::new(&legacy_idx_path).exists() {
            // Fall through to legacy file — but we need to match the index name
            // against the metadata, so read the meta first
            if let Ok(meta_json) = std::fs::read_to_string(&legacy_meta_path) {
                #[derive(serde::Deserialize)]
                struct IndexMeta {
                    column_name: String,
                }
                if let Ok(meta) = serde_json::from_str::<IndexMeta>(&meta_json) {
                    let generated_name = format!("idx_{}_{}", table_name, meta.column_name);
                    if generated_name.eq_ignore_ascii_case(index_name) {
                        std::fs::remove_file(&legacy_idx_path)?;
                        let _ = std::fs::remove_file(&legacy_meta_path);
                    } else {
                        if if_exists {
                            println!("Index '{}' does not exist (IF EXISTS specified, skipping).", index_name);
                            return Ok(());
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("Index '{}' not found on table '{}'", index_name, table_name),
                        ));
                    }
                } else {
                    if if_exists {
                        println!("Index '{}' does not exist (IF EXISTS specified, skipping).", index_name);
                        return Ok(());
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Index file found but metadata invalid for '{}'", index_name),
                    ));
                }
            } else {
                // Legacy file exists but no meta — check via sys_indexes
                // If no named file exists and no legacy match, it's an error
                if if_exists {
                    println!("Index '{}' does not exist (IF EXISTS specified, skipping).", index_name);
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Index '{}' not found on table '{}'", index_name, table_name),
                ));
            }
        } else {
            if if_exists {
                println!("Index '{}' does not exist (IF EXISTS specified, skipping).", index_name);
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Index file not found for table '{}'", table_name),
            ));
        }

        // Clean up sys_indexes
        if let Err(e) = storage_manager::backend::system_table::delete_index_metadata(db, table_name, index_name) {
            eprintln!(
                "[DropIndex] Warning: failed to clean up sys_indexes for '{}': {}",
                index_name, e
            );
        }

        println!("Dropped index '{}' from table '{}'.", index_name, table_name);
    } else {
        // No table name — try to find the index file by scanning
        let base_dir = format!("database/base/{}", db);
        let base_path = std::path::Path::new(&base_dir);
        if !base_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Database directory '{}' not found", base_dir),
            ));
        }

        let mut found = false;

        // Scan the database directory for named index files: {table}.{index_name}.idx
        if let Ok(entries) = std::fs::read_dir(base_path) {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                // Match {table}.{index_name}.idx pattern
                if fname.ends_with(".idx") && !fname.ends_with(".idx.meta") {
                    // Try to parse as named index: table.index_name.idx
                    let without_suffix = fname.strip_suffix(".idx").unwrap_or("");
                    if let Some(dot_pos) = without_suffix.rfind('.') {
                        let tbl_name = &without_suffix[..dot_pos];
                        let idx_name_part = &without_suffix[dot_pos + 1..];
                        if idx_name_part.eq_ignore_ascii_case(index_name) {
                            let idx_path = entry.path();
                            let meta_path = format!("database/base/{}/{}", db, fname.replace(".idx", ".idx.meta"));
                            std::fs::remove_file(&idx_path)?;
                            let _ = std::fs::remove_file(&meta_path);

                            if let Err(e) = storage_manager::backend::system_table::delete_index_metadata(
                                db, tbl_name, index_name,
                            ) {
                                eprintln!(
                                    "[DropIndex] Warning: failed to clean up sys_indexes: {}",
                                    e
                                );
                            }

                            println!("Dropped index '{}' from table '{}'.", index_name, tbl_name);
                            found = true;
                            break;
                        }
                    } else {
                        // Legacy file: {table}.idx — read meta to check name
                        let tbl_name = without_suffix;
                        let meta_path = format!("database/base/{}/{}.idx.meta", db, tbl_name);
                        if let Ok(meta_json) = std::fs::read_to_string(&meta_path) {
                            #[derive(serde::Deserialize)]
                            struct IndexMeta {
                                column_name: String,
                            }
                            if let Ok(meta) = serde_json::from_str::<IndexMeta>(&meta_json) {
                                let generated_name = format!("idx_{}_{}", tbl_name, meta.column_name);
                                if generated_name.eq_ignore_ascii_case(index_name) {
                                    let idx_path = entry.path();
                                    std::fs::remove_file(&idx_path)?;
                                    let _ = std::fs::remove_file(&meta_path);

                                    if let Err(e) = storage_manager::backend::system_table::delete_index_metadata(
                                        db, tbl_name, index_name,
                                    ) {
                                        eprintln!(
                                            "[DropIndex] Warning: failed to clean up sys_indexes: {}",
                                            e
                                        );
                                    }

                                    println!("Dropped index '{}' from table '{}'.", index_name, tbl_name);
                                    found = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        if !found {
            if if_exists {
                println!("Index '{}' does not exist (IF EXISTS specified, skipping).", index_name);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Index '{}' not found in database '{}'", index_name, db),
                ));
            }
        }
    }

    Ok(())
}

