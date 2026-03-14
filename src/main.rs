use rook_parser::parse_sql;
use std::io::{self, Write};
use storage_manager::catalog::{create_database, create_table, show_databases, show_tables};

mod db;

fn main() -> io::Result<()> {
    println!("--------------------------------------");
    println!("Welcome to RookDB");
    println!("--------------------------------------\n");

    // Initialize storage manager catalog
    let mut catalog = db::initialize_catalog();

    // Track current database
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

        if input.is_empty() {
            continue;
        }

        match parse_sql(input) {
            Ok(json) => {
                let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

                let category = parsed["category"].as_str().unwrap_or("");
                let stmt_type = parsed["type"].as_str().unwrap_or("");
                let params = &parsed["params"];

                if category == "DQL" && stmt_type == "ShowDatabases" {
                    show_databases(&catalog);
                } else if category == "DDL" && stmt_type == "CreateDatabase" {
                    let db_name = params["database"].as_str().unwrap_or("");
                    create_database(&mut catalog, db_name);
                    println!("Database '{}' created successfully.", db_name);
                } else if category == "DDL" && stmt_type == "USEDatabase" {
                    let raw = params["database"].as_str().unwrap_or("");
                    let db_name = raw.split_whitespace().last().unwrap_or("");

                    current_db = Some(db_name.to_string());
                    if catalog.databases.is_empty() {
                        println!("No databases found.");
                    } else if catalog.databases.contains_key(db_name) {
                        current_db = Some(db_name.to_string());
                        println!("Database '{}' selected.", db_name);
                    } else {
                        println!("Database '{}' does not exist.", db_name);
                    }
                } else if category == "DQL" && stmt_type == "ShowTables" {
                    if let Some(db_name) = &current_db {
                        show_tables(&catalog, db_name);
                    } else {
                        println!("No database selected. Use 'USE <database>' first.");
                    }
                } else if category == "DDL" && stmt_type == "CreateTable" {
                    let db = match &current_db {
                        Some(db) => db.clone(),
                        None => {
                            println!("No database selected. Use 'USE <database>' first.");
                            continue;
                        }
                    };

                    let table_name = params["table"].as_str().unwrap_or("");

                    let mut columns = Vec::new();

                    if let Some(cols) = params["columns"].as_array() {
                        for col in cols {
                            let name = col["name"].as_str().unwrap_or("").to_string();
                            let data_type = col["data_type"].as_str().unwrap_or("").to_string();

                            columns.push(storage_manager::catalog::Column { name, data_type });
                        }
                    }

                    create_table(&mut catalog, &db, table_name, columns);

                    println!("Table '{}' created successfully.", table_name);
                } else {
                    println!("{}", json);
                }
            }
            Err(err) => {
                println!("Parse error: {}", err);
            }
        }
    }

    Ok(())
}
