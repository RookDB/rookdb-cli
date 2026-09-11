use std::io;
use std::str::FromStr;

use rook_ast::*;
use storage_manager::catalog::{create_database, create_table, Catalog};use storage_manager::catalog::Column;
use storage_manager::catalog::Constraints;
use storage_manager::types::DataType;
use storage_manager::executor::create_index;

use crate::db;
use crate::handlers::helpers::save_table_constraint;

/// Handle CREATE DATABASE
pub fn handle_create_database(
    catalog: &mut Catalog,
    _current_db: &mut Option<String>,
    params: &CreateDatabasePlan,
) -> io::Result<()> {
    create_database(catalog, &params.database);
    println!("Database '{}' created successfully.", params.database);
    Ok(())
}

/// Handle DROP DATABASE
pub fn handle_drop_database(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &DropDatabasePlan,
) -> io::Result<()> {
    let db_name = &params.database;

    // Defense-in-depth: the name is about to be used in `remove_dir_all`.
    if let Err(e) = storage_manager::backend::name_validation::validate_database_name(db_name) {
        eprintln!("{}", e);
        return Ok(());
    }

    if !catalog.databases.contains_key(db_name) {
        if params.if_exists {
            println!("Database '{}' does not exist (IF EXISTS specified, skipping).", db_name);
        } else {
            println!("Database '{}' does not exist.", db_name);
        }
        return Ok(());
    }

    catalog.databases.remove(db_name);

    let db_dir = format!("database/base/{}", db_name);
    let _ = std::fs::remove_dir_all(&db_dir);

    if let Err(e) = storage_manager::backend::system_table::delete_database_metadata(db_name) {
        eprintln!("[DropDatabase] Warning: failed to clean up system table metadata: {}", e);
    }

    if let Err(e) = storage_manager::catalog::save_catalog(catalog) {
        eprintln!("[DropDatabase] Warning: failed to save catalog: {}", e);
    }

    if let Some(ref cur) = *current_db
        && cur == db_name {
            *current_db = None;
        }

    println!("Database '{}' dropped successfully.", db_name);
    Ok(())
}

/// Handle USE DATABASE
pub fn handle_use_database(
    catalog: &Catalog,
    current_db: &mut Option<String>,
    db_name: &str,
) -> io::Result<()> {
    if catalog.databases.is_empty() {
        println!("No databases found.");
    } else if catalog.databases.contains_key(db_name) {
        *current_db = Some(db_name.to_string());
        println!("Database '{}' selected.", db_name);
    } else {
        println!("Database '{}' does not exist.", db_name);
    }
    Ok(())
}

/// Handle CREATE TABLE
pub fn handle_create_table(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &CreateTablePlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };

    let mut columns = Vec::new();

    for col in &params.columns {
        let data_type = match DataType::from_str(&col.data_type) {
            Ok(dt) => dt,
            Err(err) => {
                println!(
                    "Unknown type '{}': {}. Supported: SMALLINT, INT, BIGINT, REAL, DOUBLE PRECISION, NUMERIC(p,s), DECIMAL(p,s), BOOLEAN, CHAR(n), CHARACTER(n), VARCHAR(n), DATE, TIME, TIMESTAMP, BIT(n)",
                    col.data_type, err
                );
                continue;
            }
        };

        let constraint_strs: Vec<&str> = col.constraints.iter().map(|s| s.as_str()).collect();
        let not_null = constraint_strs.iter().any(|c| c.eq_ignore_ascii_case("NOT NULL") || c.eq_ignore_ascii_case("PRIMARY KEY"));
        let unique = constraint_strs.iter().any(|c| c.eq_ignore_ascii_case("UNIQUE") || c.eq_ignore_ascii_case("PRIMARY KEY"));
        let _has_primary = constraint_strs.iter().any(|c| c.eq_ignore_ascii_case("PRIMARY KEY"));

        let default_val = constraint_strs.iter().find_map(|c| {
            let upper = c.to_uppercase();
            if let Some(_val) = upper.strip_prefix("DEFAULT ") {
                let raw_val = &c["DEFAULT ".len()..];
                Some(raw_val.to_string())
            } else if let Some(val) = upper.strip_prefix("DEFAULT(") {
                val.rfind(')').map(|end| val[..end].to_string())
            } else {
                None
            }
        });
        let default_dv = default_val.as_ref().and_then(|v| {
            storage_manager::executor::create_index::parse_string_to_value(&data_type, v).ok()
        });

        let check_expr = constraint_strs.iter().find_map(|c| {
            let upper = c.to_uppercase();
            if upper.starts_with("CHECK(") {
                if let Some(end) = upper.rfind(')') {
                    let expr = c["CHECK(".len()..end].to_string();
                    Some(expr)
                } else {
                    None
                }
            } else {
                None
            }
        });

        columns.push(Column {
            name: col.name.clone(),
            data_type,
            nullable: !not_null,
            constraints: Constraints {
                not_null,
                unique,
                default: default_dv,
                check: check_expr,
            },
        });
    }

    create_table(catalog, &db, &params.table, columns);
    println!("Table '{}' created successfully.", params.table);

    for tc in &params.constraints {
        save_table_constraint(&db, &params.table, &tc.definition);
    }

    // Auto-create B+ Tree index on PRIMARY KEY columns
    for col in &params.columns {
        let has_pk = col.constraints.iter().any(|c| c.eq_ignore_ascii_case("PRIMARY KEY"));
        if has_pk {
            let index_name = format!("pk_{}_{}", params.table, col.name);
            if let Err(e) = create_index(catalog, &db, &params.table, &index_name, std::slice::from_ref(&col.name)) {
                eprintln!("Warning: failed to auto-create PRIMARY KEY index: {}", e);
            }
        }
    }

    Ok(())
}

/// Handle DROP TABLE
pub fn handle_drop_table(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &DropTablePlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };
    db::execute_drop_table(catalog, &db, &params.table, params.if_exists, params.cascade)
}

/// Handle ALTER TABLE
pub fn handle_alter_table(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &AlterTablePlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };
    db::execute_alter_table(catalog, &db, params)
}

/// Handle CREATE VIEW
pub fn handle_create_view(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &CreateViewPlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };
    db::execute_create_view(catalog, &db, params)
}

/// Handle DROP VIEW
pub fn handle_drop_view(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &DropViewPlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };
    db::execute_drop_view(catalog, &db, &params.name, params.if_exists)
}

/// Handle TRUNCATE TABLE
/// Handle TRUNCATE
pub fn handle_truncate(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &TruncatePlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };
    db::execute_truncate(catalog, &db, &params.table)
}

/// Handle VACUUM — reclaim space from soft-deleted rows and rebuild indexes.
pub fn handle_vacuum(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &VacuumPlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };

    match storage_manager::backend::executor::vacuum::vacuum_table(catalog, &db, &params.table) {
        Ok(stats) => {
            println!(
                "VACUUM '{}.{}' complete: {} page(s) compacted, {} dead tuple(s) reclaimed, {} index(es) rebuilt.",
                db,
                params.table,
                stats.pages_compacted,
                stats.dead_tuples_before,
                stats.indexes_rebuilt
            );
            if stats.pages_compacted == 0 {
                println!("Table is already clean — nothing to do.");
            }
        }
        Err(e) => println!("VACUUM failed: {}", e),
    }
    Ok(())
}

/// Handle CREATE TABLE AS SELECT
pub fn handle_create_table_as_select(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &CreateTableAsSelectPlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };
    db::execute_create_table_as_select(catalog, &db, params)
}

/// Handle DROP INDEX
pub fn handle_drop_index(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &DropIndexPlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };
    db::execute_drop_index(catalog, &db, &params.index_name, &params.table_name, params.if_exists)
}

/// Handle CREATE INDEX
pub fn handle_create_index(
    catalog: &mut Catalog,
    current_db: &mut Option<String>,
    params: &CreateIndexPlan,
) -> io::Result<()> {
    let db = match current_db {
        Some(db) => db.clone(),
        None => {
            println!("No database selected. Use 'USE <database>' first.");
            return Ok(());
        }
    };

    match create_index(catalog, &db, &params.table_name, &params.index_name, &params.columns) {
        Ok(count) => {
            println!(
                "Created index '{}' on {}.{}({}) with {} entries.",
                params.index_name, db, params.table_name, params.columns.join(","), count
            );
        }
        Err(e) => {
            println!("Failed to create index: {}", e);
        }
    }

    Ok(())
}
