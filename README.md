# Rook CLI

An interactive SQL CLI for RookDB that takes SQL statements as input and uses `rook-parser` and the RookDB storage engine to process and display results.

The input to the program is a SQL statement typed in the terminal (e.g., `SELECT`, `CREATE`, `INSERT`, etc.).  
The CLI parses the query using `rook-parser` and prints the generated Abstract Syntax Tree (AST) along with the interpreted SQL statement.  
Type `exit` or `quit` to terminate the CLI session.