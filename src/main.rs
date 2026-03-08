

use std::io::{self, Write};
use rook_parser::parse_sql;
use rook_parser::ast::Statement;

fn main() -> io::Result<()> {
    println!("--------------------------------------");
    println!("Welcome to RookDB");
    println!("--------------------------------------\n");

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

        match parse_sql(input) {
            Ok(statement) => {
                println!("{:#?}", statement);

                match statement {
                    Statement::Create(stmt) => println!("CREATE => {}", stmt),
                    Statement::Select(stmt) => println!("SELECT => {}", stmt),
                    Statement::Insert(stmt) => println!("INSERT => {}", stmt),
                    Statement::Update(stmt) => println!("UPDATE => {}", stmt),
                    Statement::Delete(stmt) => println!("DELETE => {}", stmt),
                    Statement::Drop(stmt) => println!("DROP => {}", stmt),
                    Statement::Alter(stmt) => println!("ALTER => {}", stmt),
                }
            }
            Err(err) => {
                println!("Parse error: {}", err);
            }
        }
    }

    Ok(())
}
