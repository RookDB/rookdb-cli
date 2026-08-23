use rook_ast::{ArithOp, ConstantValue, ExprNode};

use crate::convert;

/// Parse a table-level constraint definition string and save it to `sys_constraints`.
///
/// Handles definitions produced by sqlparser such as:
///   "PRIMARY KEY (id)", "UNIQUE (name)", "UNIQUE",
///   "FOREIGN KEY (user_id) REFERENCES users(id)",
///   "CHECK (age > 0)"
pub fn save_table_constraint(db: &str, table: &str, definition: &str) {
    // ASCII-only folding: keyword matching needs nothing beyond A-Z, and it
    // guarantees `upper.len() == definition.len()` so offsets into the
    // original string stay valid (see the FOREIGN KEY arm below).
    let upper = definition.to_ascii_uppercase();

    // PRIMARY KEY (cols...)
    if let Some(rest) = upper.strip_prefix("PRIMARY KEY (") {
        if let Some(end) = rest.rfind(')') {
            let cols = rest[..end].to_string();
            let _ = storage_manager::backend::system_table::insert_constraint_metadata(
                db, table, "PRIMARY KEY", &cols, None, None,
            );
        }
    }
    // UNIQUE (cols...)
    else if let Some(rest) = upper.strip_prefix("UNIQUE (") {
        if let Some(end) = rest.rfind(')') {
            let cols = rest[..end].to_string();
            let _ = storage_manager::backend::system_table::insert_constraint_metadata(
                db, table, "UNIQUE", &cols, None, None,
            );
        }
    }
    // UNIQUE without columns (table-level shorthand)
    else if upper == "UNIQUE" {
        let _ = storage_manager::backend::system_table::insert_constraint_metadata(
            db, table, "UNIQUE", "", None, None,
        );
    }
    // FOREIGN KEY (cols) REFERENCES ref_table(ref_cols) [ON DELETE CASCADE|RESTRICT]
    //
    // Keywords are matched on an ASCII-uppercased copy, but identifiers are
    // sliced from the ORIGINAL definition: ref_table/ref_cols feed
    // case-sensitive file and catalog lookups during enforcement, so their
    // letter-case must survive verbatim.
    else if upper.starts_with("FOREIGN KEY (") {
        // to_ascii_uppercase is length-preserving by construction, so
        // offsets from `upper` always address the same characters in
        // `definition` (plain to_uppercase is NOT: 'ß' → "SS" would shift
        // every later offset).
        let off = "FOREIGN KEY (".len();
        let rest_u = &upper[off..];
        if let Some(end_paren) = rest_u.find(')') {
            let cols = definition[off..off + end_paren].to_string();
            let after_paren = definition[off + end_paren + 1..].trim_start();
            if let Some(ref_rest) = after_paren.strip_prefix("REFERENCES ") {
                if let Some(ref_start) = ref_rest.find('(') {
                    let ref_table_name = ref_rest[..ref_start].trim().to_string();
                    let ref_cols_rest = &ref_rest[ref_start + 1..];
                    if let Some(ref_end) = ref_cols_rest.rfind(')') {
                        let ref_cols = ref_cols_rest[..ref_end].to_string();
                        let remaining_after_ref = ref_cols_rest[ref_end + 1..].trim();
                        let mut delete_action = "";
                        let mut update_action = "";
                        let mut rem = remaining_after_ref.to_uppercase();
                        if let Some(after) = rem.strip_prefix("ON DELETE ") {
                            if after.starts_with("CASCADE") {
                                delete_action = " ON DELETE CASCADE";
                            } else if after.starts_with("SET NULL") {
                                delete_action = " ON DELETE SET NULL";
                            }
                            let skip_to = after.find(|c: char| c.is_whitespace()).unwrap_or(after.len());
                            rem = after[skip_to..].trim().to_string();
                        }
                        if let Some(after) = rem.strip_prefix("ON UPDATE ") {
                            if after.starts_with("CASCADE") {
                                update_action = " ON UPDATE CASCADE";
                            } else if after.starts_with("SET NULL") {
                                update_action = " ON UPDATE SET NULL";
                            }
                        }
                        let constr_type = format!("FOREIGN KEY{}{}", delete_action, update_action);
                        let _ = storage_manager::backend::system_table::insert_constraint_metadata(
                            db, table, &constr_type, &cols,
                            Some(&ref_table_name), Some(&ref_cols),
                        );
                    }
                } else {
                    let ref_table_name = ref_rest.split_whitespace().next().unwrap_or(ref_rest).to_string();
                    let _ = storage_manager::backend::system_table::insert_constraint_metadata(
                        db, table, "FOREIGN KEY", &cols,
                        Some(&ref_table_name), Some(""),
                    );
                }
            }
        }
    }
    // CHECK (expr)
    else if upper.starts_with("CHECK (") || upper.starts_with("CHECK(") {
        let check_start = if upper.starts_with("CHECK(") { 6 } else { 7 };
        let remaining = &definition[check_start..];
        if let Some(end) = find_matching_paren(remaining) {
            let expr = remaining[1..end].trim();
            if !expr.is_empty() {
                let _ = storage_manager::backend::system_table::insert_constraint_metadata(
                    db, table, "CHECK", expr, None, None,
                );
            }
        }
    }
}

/// Find the index of the matching close-paren for a string that starts with `(...)`.
/// Properly handles nested parentheses by counting depth.
pub fn find_matching_paren(s: &str) -> Option<usize> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth: i32 = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Convert an `ExprNode` to a value string for INSERT processing.
pub fn value_expr_to_string(expr: &ExprNode) -> String {
    match expr {
        ExprNode::Constant(cv) => convert::constant_to_raw_string(cv),
        // Handle negative literals: the parser converts -5 to 0 - 5 (Binary with Sub)
        ExprNode::Binary { left, op: ArithOp::Sub, right }
            if matches!(**left, ExprNode::Constant(ConstantValue::Int(0))) =>
        {
            format!("-{}", value_expr_to_string(right))
        }
        ExprNode::Case { when_then_pairs, else_result } => {
            // CASE expressions in VALUES are not supported; return a placeholder
            let _ = (when_then_pairs, else_result);
            "NULL".to_string()
        }
        _ => format!("{:?}", expr), // fallback
    }
}

/// Convert an `ExprNode` to a debug/display string for SET clause building.
pub fn expr_to_debug_string(expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column(name) => name.clone(),
        ExprNode::Compound(parts) => parts.join("."),
        ExprNode::Constant(cv) => convert::constant_to_raw_string(cv),
        ExprNode::Binary { left, op, right } => {
            let op_str = match op {
                ArithOp::Add => "+",
                ArithOp::Sub => "-",
                ArithOp::Mul => "*",
                ArithOp::Div => "/",
            };
            format!(
                "{} {} {}",
                expr_to_debug_string(left),
                op_str,
                expr_to_debug_string(right)
            )
        }
        ExprNode::Cast { expr, data_type } => format!(
            "CAST({} AS {})",
            expr_to_debug_string(expr),
            data_type
        ),
        ExprNode::ScalarSubquery(_) => "(scalar subquery)".to_string(),
        ExprNode::Function { name, args, .. } => {
            let arg_strs: Vec<String> = args
                .iter()
                .map(|a| match a {
                    rook_ast::FunctionArg::Star => "*".to_string(),
                    rook_ast::FunctionArg::Expr(e) => expr_to_debug_string(e),
                })
                .collect();
            format!("{}({})", name, arg_strs.join(", "))
        }
        ExprNode::Case {
            when_then_pairs,
            else_result,
        } => {
            let parts: Vec<String> = when_then_pairs
                .iter()
                .map(|(cond, res)| {
                    format!(
                        "WHEN {} THEN {}",
                        expr_to_debug_string(cond),
                        expr_to_debug_string(res)
                    )
                })
                .collect();
            let else_part = else_result
                .as_ref()
                .map(|e| format!(" ELSE {}", expr_to_debug_string(e)))
                .unwrap_or_default();
            format!("CASE {} END{}", parts.join(" "), else_part)
        }
    }
}
