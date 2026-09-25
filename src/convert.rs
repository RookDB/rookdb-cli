//! Conversion helpers: `rook_ast` → `storage_manager` types.
//!
//! These functions bridge the typed AST produced by `rook-parser` into the
//! existing execution types in `storage_manager`.

use rook_ast::ConstantValue;

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
