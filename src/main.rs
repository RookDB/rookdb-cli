use rook_parser::parse_sql;
use std::io::{self, Write};
use storage_manager::catalog::{create_database, show_databases};

mod db;

fn main() -> io::Result<()> {
    println!("--------------------------------------");
    println!("Welcome to RookDB");
    println!("--------------------------------------\n");

    // Initialize storage manager catalog
    let mut catalog = db::initialize_catalog();

    // show_databases(&catalog);

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
