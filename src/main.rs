use rook_parser::parse_sql;
use std::io::{self, Write};
use std::str::FromStr;
use storage_manager::catalog::{create_database, create_table, show_databases, show_tables};
use storage_manager::catalog::{Column, Constraints};
use storage_manager::insert_single_tuple;
use storage_manager::types::DataType;

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

                            let data_type_str = col["data_type"].as_str().unwrap_or("").trim();

                            let data_type = match DataType::from_str(data_type_str) {
                                Ok(dt) => dt,
                                Err(err) => {
                                    println!(
            "Unknown type '{}': {}. Supported: SMALLINT, INT, BIGINT, REAL, DOUBLE PRECISION, NUMERIC(p,s), DECIMAL(p,s), BOOLEAN, CHAR(n), CHARACTER(n), VARCHAR(n), DATE, TIME, TIMESTAMP, BIT(n)",
            data_type_str,
            err
        );
                                    continue;
                                }
                            };

                            let nullable = col["nullable"].as_bool().unwrap_or(true);

                            let constraints = Constraints {
                                not_null: col["constraints"]["not_null"].as_bool().unwrap_or(false),

                                unique: col["constraints"]["unique"].as_bool().unwrap_or(false),

                                default: None,
                                check: None,
                            };

                            columns.push(Column {
                                name,
                                data_type,
                                nullable,
                                constraints,
                            });
                        }
                    }

                    create_table(&mut catalog, &db, table_name, columns);

                    println!("Table '{}' created successfully.", table_name);
                } else if category == "DML" && stmt_type == "Insert" {
                    let db = match &current_db {
                        Some(db) => db,
                        None => {
                            println!("No database selected. Use 'USE <database>' first.");
                            continue;
                        }
                    };

                    let table_name = params["table"].as_str().unwrap_or("");

                    if let Some(rows) = params["values"].as_array() {
                        for row in rows {
                            if let Some(values_array) = row.as_array() {
                                let values: Vec<&str> =
                                    values_array.iter().filter_map(|v| v.as_str()).collect();

                                match insert_single_tuple(&catalog, db, table_name, &values) {
                                    Ok(true) => {
                                        println!("1 row inserted.");
                                    }
                                    Ok(false) => {
                                        println!("Insert failed.");
                                    }
                                    Err(e) => {
                                        println!("Insert error: {}", e);
                                    }
                                }
                            }
                        }
                    }
                } else if category == "DQL" && stmt_type == "Select" {
                    let db = match &current_db {
                        Some(db) => db,
                        None => {
                            println!("No database selected. Use 'USE <database>' first.");
                            continue;
                        }
                    };

                    // Extract table name from params
                    let tables = params["tables"]
                        .as_array()
                        .and_then(|t| t.first())
                        .and_then(|t| t.as_str())
                        .unwrap_or("");

                    if tables.is_empty() {
                        println!("No table specified in SELECT query.");
                        continue;
                    }

                    // Extract WHERE clause if it exists
                    let where_clause = params["filters"]
                        .as_array()
                        .and_then(|f| f.first())
                        .and_then(|f| f.as_str());

                    // Execute SELECT
                    match db::execute_select(&catalog, db, tables, where_clause) {
                        Ok(_) => {
                            // Output already printed by execute_select
                        }
                        Err(e) => {
                            println!("SELECT error: {}", e);
                        }
                    }
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
