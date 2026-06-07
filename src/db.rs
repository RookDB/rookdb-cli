use std::fs::OpenOptions;
use std::path::Path;

use storage_manager::catalog::{init_catalog, load_catalog, Catalog};
use storage_manager::disk::read_page;
use storage_manager::executor::selection::{filter_tuples, SelectionExecutor};
use storage_manager::page::{Page, ITEM_ID_SIZE, PAGE_HEADER_SIZE};
use storage_manager::query::build_predicate_from_sql;
use storage_manager::table::page_count;
use storage_manager::types::deserialize_nullable_row;

pub fn initialize_catalog() -> Catalog {
    init_catalog();
    load_catalog()
}

/// Execute a SELECT query (SELECT * or with WHERE clause)
pub fn execute_select(
    catalog: &Catalog,
    db: &str,
    table: &str,
    where_clause: Option<&str>,
) -> Result<(), String> {
    // Validate database and table exist
    let db_obj = catalog
        .databases
        .get(db)
        .ok_or_else(|| format!("Database '{}' not found", db))?;

    let table_schema = db_obj
        .tables
        .get(table)
        .ok_or_else(|| format!("Table '{}' not found in database '{}'", table, db))?;

    // Check if table file exists
    let path = format!("database/base/{}/{}.dat", db, table);
    if !Path::new(&path).exists() {
        return Err(format!("Table file not found: '{}'", path));
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("Failed to open table file: {}", e))?;

    // Build predicate from WHERE clause
    let predicate = if let Some(where_str) = where_clause {
        if where_str.is_empty() || where_str.eq_ignore_ascii_case("1=1") {
            // SELECT * with no WHERE clause - match all rows
            storage_manager::executor::selection::Predicate::IsNotNull(Box::new(
                storage_manager::executor::selection::Expr::Constant(
                    storage_manager::executor::selection::Constant::Null,
                ),
            ))
        } else {
            // Wrap WHERE clause in a SELECT statement for proper parsing
            let full_sql = format!("SELECT * FROM _ WHERE {}", where_str);
            build_predicate_from_sql(&full_sql)
                .map_err(|e| format!("Failed to parse WHERE clause: {}", e))?
        }
    } else {
        // SELECT * with no WHERE clause - match all rows
        storage_manager::executor::selection::Predicate::IsNotNull(Box::new(
            storage_manager::executor::selection::Expr::Constant(
                storage_manager::executor::selection::Constant::Null,
            ),
        ))
    };

    // Create SelectionExecutor
    let executor = SelectionExecutor::new(predicate, table_schema.clone())
        .map_err(|e| format!("Failed to create executor: {}", e))?;

    // Read all raw tuples from data pages
    let total_pages =
        page_count(&mut file).map_err(|e| format!("Failed to get page count: {}", e))?;
    let columns = &table_schema.columns;
    let schema_types: Vec<_> = columns.iter().map(|c| c.data_type.clone()).collect();

    println!("\n=== Tuples in '{}.{}' ===", db, table);
    println!("Total pages: {}", total_pages);

    let header: Vec<String> = columns
        .iter()
        .map(|c| format!("{} ({})", c.name, c.data_type))
        .collect();
    println!("{}", header.join(" | "));

    let mut raw_tuples: Vec<Vec<u8>> = Vec::new();

    // Skip page 0 (table header), iterate data pages
    for page_num in 1..total_pages {
        let mut page = Page::new();
        read_page(&mut file, &mut page, page_num)
            .map_err(|e| format!("Failed to read page {}: {}", page_num, e))?;

        let lower = u32::from_le_bytes(page.data[0..4].try_into().unwrap());
        let num_items = (lower - PAGE_HEADER_SIZE) / ITEM_ID_SIZE;

        for i in 0..num_items {
            let base = (PAGE_HEADER_SIZE + i * ITEM_ID_SIZE) as usize;
            let offset = u32::from_le_bytes(page.data[base..base + 4].try_into().unwrap());
            let length =
                u16::from_le_bytes(page.data[base + 4..base + 6].try_into().unwrap()) as u32;

            let flags = u16::from_le_bytes(page.data[base + 6..base + 8].try_into().unwrap());

            // Skip deleted tuples
            if (flags & storage_manager::page::SLOT_FLAG_DELETED) != 0 {
                continue;
            }
            let tuple_bytes = page.data[offset as usize..(offset + length) as usize].to_vec();
            raw_tuples.push(tuple_bytes);
        }
    }

    // Apply predicate filtering
    let matching = filter_tuples(&executor, &raw_tuples)
        .map_err(|e| format!("Failed to filter tuples: {}", e))?;

    // Print filtered tuples
    for (i, tuple_bytes) in matching.iter().enumerate() {
        print!("Tuple {}: ", i + 1);
        match deserialize_nullable_row(&schema_types, tuple_bytes) {
            Ok(values) => {
                for (col, val_opt) in columns.iter().zip(values.iter()) {
                    match val_opt {
                        Some(val) => print!("{}={} ", col.name, val),
                        None => print!("{}=NULL ", col.name),
                    }
                }
            }
            Err(e) => print!("<decode-error: {}> ", e),
        }
        println!();
    }

    println!("\n=== End of tuples (found {}) ===\n", matching.len());
    Ok(())
}
