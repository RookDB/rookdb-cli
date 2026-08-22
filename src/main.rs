use rook_ast::*;
use rook_parser::parse_sql;
use std::io::{self, Write};

mod convert;
mod db;
mod handlers;

fn main() -> io::Result<()> {
    println!("--------------------------------------");
    println!("Welcome to RookDB");
    println!("--------------------------------------\n");

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

        if let Err(e) = (|| -> io::Result<()> {
            match parse_sql(input) {
                Ok(plan) => match plan {
                    QueryPlan::ShowDatabases => handlers::dql::handle_show_databases(&catalog, &mut current_db)?,
                    QueryPlan::DropDatabase(ref p) => handlers::ddl::handle_drop_database(&mut catalog, &mut current_db, p)?,
                    QueryPlan::CreateDatabase(ref p) => handlers::ddl::handle_create_database(&mut catalog, &mut current_db, p)?,
                    QueryPlan::UseDatabase(ref name) => handlers::ddl::handle_use_database(&catalog, &mut current_db, name)?,
                    QueryPlan::ShowTables => handlers::dql::handle_show_tables(&catalog, &mut current_db)?,
                    QueryPlan::CreateTable(ref p) => handlers::ddl::handle_create_table(&mut catalog, &mut current_db, p)?,
                    QueryPlan::Insert(ref p) => handlers::dml::handle_insert(&mut catalog, &mut current_db, &plan, p)?,
                    QueryPlan::Select(_) => handlers::dql::handle_select(&mut catalog, &mut current_db, &plan)?,
                    QueryPlan::Update(ref p) => handlers::dml::handle_update(&mut catalog, &mut current_db, p)?,
                    QueryPlan::DropTable(ref p) => handlers::ddl::handle_drop_table(&mut catalog, &mut current_db, p)?,
                    QueryPlan::AlterTable(ref p) => handlers::ddl::handle_alter_table(&mut catalog, &mut current_db, p)?,
                    QueryPlan::CreateView(ref p) => handlers::ddl::handle_create_view(&mut catalog, &mut current_db, p)?,
                    QueryPlan::DropView(ref p) => handlers::ddl::handle_drop_view(&mut catalog, &mut current_db, p)?,
                    QueryPlan::Truncate(ref p) => handlers::ddl::handle_truncate(&mut catalog, &mut current_db, p)?,
                    QueryPlan::SetOperation(_) => handlers::dql::handle_set_operation(&mut catalog, &mut current_db, &plan)?,
                    QueryPlan::CreateTableAsSelect(ref p) => handlers::ddl::handle_create_table_as_select(&mut catalog, &mut current_db, p)?,
                    QueryPlan::DropIndex(ref p) => handlers::ddl::handle_drop_index(&mut catalog, &mut current_db, p)?,
                    QueryPlan::CreateIndex(ref p) => handlers::ddl::handle_create_index(&mut catalog, &mut current_db, p)?,
                    QueryPlan::Delete(ref p) => handlers::dml::handle_delete(&mut catalog, &mut current_db, p)?,
                    QueryPlan::Unknown(ref msg) => println!("Unsupported SQL statement: {}", msg),
                },
                Err(err) => println!("Parse error: {}", err),
            }
            Ok(())
        })() {
            // Catch errors and print them instead of propagating — keeps the
            // REPL alive for the next statement and lets tests check error
            // messages via stdout (not just exit codes).
            eprintln!("Error: {}", e);
        }
    }

    Ok(())
}
