use std::io::{self, Write};
use rook_parser::parse_sql;

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
                println!("{}", json);
            }
            Err(err) => {
                println!("Parse error: {}", err);
            }
        }
    }

    Ok(())
}