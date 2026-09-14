use oxc_allocator::Allocator;
use oxc_ast::ast::{BinaryOperator, Expression};
use oxc_str::{JSStr, JSStrBuilder};

/// Resolve a side-effect-free string expression made from string literals, template literals,
/// and `+` concatenation. Returns `None` when any part cannot be determined statically.
pub fn static_string_value<'a>(
    expression: &Expression<'a>,
    allocator: &'a Allocator,
) -> Option<JSStr<'a>> {
    match expression.get_inner_expression() {
        Expression::StringLiteral(literal) => Some(literal.value),
        Expression::TemplateLiteral(template) => {
            if template.expressions.is_empty() {
                return template.single_quasi();
            }
            let mut value = JSStrBuilder::new_in(allocator);
            for (index, quasi) in template.quasis.iter().enumerate() {
                value.push_js_str(quasi.value.cooked?);
                if let Some(expr) = template.expressions.get(index) {
                    value.push_js_str(static_string_value(expr, allocator)?);
                }
            }
            Some(value.into_js_str())
        }
        Expression::BinaryExpression(binary) if binary.operator == BinaryOperator::Addition => {
            let left = static_string_value(&binary.left, allocator)?;
            let right = static_string_value(&binary.right, allocator)?;
            let mut value = JSStrBuilder::with_capacity_in(left.len() + right.len(), allocator);
            value.push_js_str(left);
            value.push_js_str(right);
            Some(value.into_js_str())
        }
        _ => None,
    }
}
