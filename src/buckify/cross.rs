use starlark_syntax::codemap::Span;
use starlark_syntax::syntax::ast::{ArgumentP, AstExpr, AstNoPayload, AstStmt, ExprP, Stmt};
use starlark_syntax::syntax::{AstModule, Dialect};

const CROSS_SELECT_EXPR: &str =
    "select({\"//platforms:cross\": [\"config//:none\"], \"DEFAULT\": []})";

pub fn patch_rust_test_target_compatible_with(buck_content: String) -> String {
    let ast = match AstModule::parse("BUCK", buck_content.clone(), &Dialect::Extended) {
        Ok(ast) => ast,
        Err(_) => return buck_content,
    };

    let mut insert_positions = Vec::new();
    collect_rust_test_insert_positions(ast.statement(), &mut insert_positions);

    if insert_positions.is_empty() {
        return buck_content;
    }

    let mut out = buck_content;
    insert_positions.sort_unstable();
    for (pos, insert) in insert_positions.into_iter().rev() {
        if pos >= out.len() {
            continue;
        }
        let insert = match insert {
            Insert::Argument => build_insert(&out, pos),
            Insert::Concatenation => format!(" + {CROSS_SELECT_EXPR}"),
        };
        out.insert_str(pos, &insert);
    }

    out
}

/// How a rust_test gets the cross-compilation selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Insert {
    /// As a new `target_compatible_with` argument, before the closing parenthesis.
    Argument,
    /// Concatenated to its own `target_compatible_with` value (e.g. from a fixup),
    /// at the value's end.
    Concatenation,
}

fn collect_rust_test_insert_positions(stmt: &AstStmt, out: &mut Vec<(usize, Insert)>) {
    match &stmt.node {
        Stmt::Statements(stmts) => {
            for s in stmts {
                collect_rust_test_insert_positions(s, out);
            }
        }
        Stmt::Expression(expr) => {
            if let Some(pos) = find_rust_test_call(expr) {
                out.push(pos);
            }
        }
        _ => {}
    }
}

fn find_rust_test_call(expr: &AstExpr) -> Option<(usize, Insert)> {
    if let ExprP::Call(callee, args) = &expr.node
        && let ExprP::Identifier(ident) = &callee.node
        && ident.node.ident == "rust_test"
    {
        if let Some(value) = call_arg(&args.args, "target_compatible_with") {
            return Some((value.span.end().get() as usize, Insert::Concatenation));
        }
        return insert_pos_before_closing_paren(expr.span).map(|pos| (pos, Insert::Argument));
    }
    None
}

fn call_arg<'a>(
    args: &'a [starlark_syntax::codemap::Spanned<ArgumentP<AstNoPayload>>],
    name: &str,
) -> Option<&'a AstExpr> {
    args.iter().find_map(|arg| match &arg.node {
        ArgumentP::Named(name_spanned, value) if name_spanned.node == name => Some(value),
        _ => None,
    })
}

fn insert_pos_before_closing_paren(span: Span) -> Option<usize> {
    let end = span.end().get() as usize;
    end.checked_sub(1)
}

fn build_insert(content: &str, insert_pos: usize) -> String {
    let needs_comma = needs_leading_comma(content, insert_pos);
    let mut out = String::new();
    if needs_comma {
        out.push(',');
    }
    out.push('\n');
    out.push_str("    target_compatible_with = ");
    out.push_str(CROSS_SELECT_EXPR);
    out.push_str(",\n");
    out
}

fn needs_leading_comma(content: &str, insert_pos: usize) -> bool {
    for ch in content[..insert_pos].chars().rev() {
        if ch.is_whitespace() {
            continue;
        }
        return ch != ',';
    }
    true
}
