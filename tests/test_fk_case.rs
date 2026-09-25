//! FK constraint definitions must preserve identifier letter-case.
//!
//! `save_table_constraint` used to slice identifiers (child cols,
//! ref_table, ref_cols) out of the *uppercased* definition string. The
//! stored ref_table feeds case-sensitive heap-file lookups during
//! enforcement (`database/base/{db}/{ref_table}.dat`), so a table created
//! as `Depts` was referenced as `DEPTS` and every valid child insert
//! failed with a phantom FK violation.
//!
//! Regression: mixed-case references must work; violations must still be
//! rejected; RESTRICT must block parent deletes while children exist.

mod common;

use common::{Workspace, expect_rows, run};

const SETUP: &[&str] = &[
    "CREATE DATABASE casetest;",
    "USE casetest;",
    "CREATE TABLE Depts (ID INT PRIMARY KEY, Name VARCHAR(20));",
    // Mixed case everywhere: child table lowercase, parent/columns mixed.
    "CREATE TABLE emps (id INT PRIMARY KEY, dept_id INT, \
         FOREIGN KEY (dept_id) REFERENCES Depts(ID));",
    "INSERT INTO Depts VALUES (1, 'Eng');",
];

#[test]
fn fk_references_resolve_with_original_letter_case() {
    let ws = Workspace::new("fkcase");

    let out = run(&ws, SETUP);
    assert!(
        !out.to_lowercase().contains("error"),
        "setup failed:\n{}",
        out
    );

    // THE regression: a valid child row against mixed-case `Depts(ID)`
    // must insert (old code looked for DEPTS.dat and failed).
    let out = run(&ws, &["USE casetest;", "INSERT INTO emps VALUES (1, 1);"]);
    assert!(
        !out.to_lowercase().contains("error") && !out.contains("Insert failed"),
        "valid FK insert failed:\n{}",
        out
    );

    // Violations are still caught: 99 does not exist in Depts.ID.
    let out = run(&ws, &["USE casetest;", "INSERT INTO emps VALUES (2, 99);"]);
    assert!(
        out.contains("Insert failed"),
        "FK violation must be rejected:\n{}",
        out
    );

    // RESTRICT (default) blocks deleting a referenced parent row.
    let out = run(&ws, &["USE casetest;", "DELETE FROM Depts WHERE ID = 1;"]);
    assert!(
        out.contains("Deleted 0 row(s)"),
        "RESTRICT must prevent the delete:\n{}",
        out
    );
    expect_rows(&ws, &["USE casetest;"], "SELECT ID FROM Depts;", &[&["1"]]);
}

#[test]
fn fk_on_delete_cascade_works_with_mixed_case_names() {
    let ws = Workspace::new("fkcase2");

    let out = run(
        &ws,
        &[
            "CREATE DATABASE casetest2;",
            "USE casetest2;",
            "CREATE TABLE Orders_2 (OID INT PRIMARY KEY);",
            "CREATE TABLE lines_2 (lid INT, oid INT, \
             FOREIGN KEY (oid) REFERENCES Orders_2(OID) ON DELETE CASCADE);",
            "INSERT INTO Orders_2 VALUES (7);",
            "INSERT INTO lines_2 VALUES (1, 7);",
            "INSERT INTO lines_2 VALUES (2, 7);",
            "DELETE FROM Orders_2 WHERE OID = 7;",
        ],
    );
    assert!(
        !out.to_lowercase().contains("error"),
        "setup failed:\n{}",
        out
    );

    // CASCADE removed both referencing child rows with the parent.
    expect_rows(&ws, &["USE casetest2;"], "SELECT lid FROM lines_2;", &[]);
}
