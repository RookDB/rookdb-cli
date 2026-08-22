//! Conversion helpers: `rook_ast` → `storage_manager` types.
//!
//! These functions bridge the typed AST produced by `rook-parser` into the
//! existing execution types in `storage_manager`.

use rook_ast::{BinaryOp, ComparisonOp, ConstantValue, ExprNode, PredicateNode};

/// Extract the WHERE clause predicate string representation (for backward compat
/// with functions that still expect string predicates).
pub fn predicate_to_debug_string(node: &PredicateNode) -> String {
    match node {
        PredicateNode::BinaryOp { left, op, right } => format!(
            "({} {} {})",
            predicate_to_debug_string(left),
            match op {
                BinaryOp::And => "AND",
                BinaryOp::Or => "OR",
            },
            predicate_to_debug_string(right)
        ),
        PredicateNode::Not(inner) => format!("NOT ({})", predicate_to_debug_string(inner)),
        PredicateNode::Compare { left, op, right } => {
            let op_str = match op {
                ComparisonOp::Eq => "=",
                ComparisonOp::Ne => "!=",
                ComparisonOp::Lt => "<",
                ComparisonOp::Le => "<=",
                ComparisonOp::Gt => ">",
                ComparisonOp::Ge => ">=",
            };
            format!(
                "{} {} {}",
                expr_node_to_debug_string(left),
                op_str,
                expr_node_to_debug_string(right)
            )
        }
        PredicateNode::IsNull(expr) => format!("{} IS NULL", expr_node_to_debug_string(expr)),
        PredicateNode::IsNotNull(expr) => format!("{} IS NOT NULL", expr_node_to_debug_string(expr)),
        PredicateNode::Between { expr, low, high } => format!(
            "{} BETWEEN {} AND {}",
            expr_node_to_debug_string(expr),
            expr_node_to_debug_string(low),
            expr_node_to_debug_string(high)
        ),
        PredicateNode::InList { expr, list } => {
            let items: Vec<String> = list.iter().map(|e| expr_node_to_debug_string(e)).collect();
            format!("{} IN ({})", expr_node_to_debug_string(expr), items.join(", "))
        }
        PredicateNode::Like { expr, pattern, .. } => {
            format!("{} LIKE '{}'", expr_node_to_debug_string(expr), pattern)
        }
        PredicateNode::Exists(_) => "EXISTS (subquery)".to_string(),
        PredicateNode::InSubquery { expr, negated, .. } => {
            let op = if *negated { "NOT IN" } else { "IN" };
            format!("{} {} (subquery)", expr_node_to_debug_string(expr), op)
        }
        PredicateNode::IsDistinctFrom { left, right } => format!(
            "{} IS DISTINCT FROM {}",
            expr_node_to_debug_string(left),
            expr_node_to_debug_string(right)
        ),
        PredicateNode::IsBoolean { expr, test, negated } => {
            let test_str = match test {
                rook_ast::BooleanTest::True => "TRUE",
                rook_ast::BooleanTest::False => "FALSE",
                rook_ast::BooleanTest::Unknown => "UNKNOWN",
            };
            if *negated {
                format!("{} IS NOT {}", expr_node_to_debug_string(expr), test_str)
            } else {
                format!("{} IS {}", expr_node_to_debug_string(expr), test_str)
            }
        }
    }
}

fn expr_node_to_debug_string(node: &ExprNode) -> String {
    match node {
        ExprNode::Column(name) => name.clone(),
        ExprNode::Compound(parts) => parts.join("."),
        ExprNode::Constant(cv) => match cv {
            ConstantValue::Null => "NULL".to_string(),
            ConstantValue::Int(i) => i.to_string(),
            ConstantValue::Float(f) => f.to_string(),
            ConstantValue::Text(s) => format!("'{}'", s),
            ConstantValue::Boolean(b) => b.to_string(),
        },
        ExprNode::Binary { left, op, right } => format!(
            "({} {:?} {})",
            expr_node_to_debug_string(left),
            op,
            expr_node_to_debug_string(right)
        ),
        ExprNode::Cast { expr, data_type } => format!(
            "CAST({} AS {})",
            expr_node_to_debug_string(expr),
            data_type
        ),
        ExprNode::ScalarSubquery(_) => "(scalar subquery)".to_string(),
        ExprNode::Function { name, args, .. } => {
            let arg_strs: Vec<String> = args
                .iter()
                .map(|a| match a {
                    rook_ast::FunctionArg::Star => "*".to_string(),
                    rook_ast::FunctionArg::Expr(e) => expr_node_to_debug_string(e),
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
                        expr_node_to_debug_string(cond),
                        expr_node_to_debug_string(res)
                    )
                })
                .collect();
            let else_part = else_result
                .as_ref()
                .map(|e| format!(" ELSE {}", expr_node_to_debug_string(e)))
                .unwrap_or_default();
            format!("CASE {} {}", parts.join(" "), else_part)
        }
    }
}

/// Convert a `ConstantValue` into a raw string value (for backward compat
/// with functions like `insert_single_tuple` that accept `&[&str]`).
pub fn constant_to_raw_string(cv: &ConstantValue) -> String {
    match cv {
        ConstantValue::Null => "NULL".to_string(),
        ConstantValue::Int(i) => i.to_string(),
        ConstantValue::Float(f) => f.to_string(),
        ConstantValue::Text(s) => s.clone(),
        ConstantValue::Boolean(b) => b.to_string(),
    }
}
