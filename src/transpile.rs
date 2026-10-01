//! memjs to JavaScript: prints the AST back out.
//!
//! memjs programs are a strict JavaScript subset, so transpilation is
//! mostly parenthesized printing. The output runs in Node — which is how
//! the differential test checks the interpreter against the reference
//! implementation.

use crate::ast::*;
use crate::interp::Item;

/// Transpiles a program to JavaScript source.
pub fn transpile(items: &[Item]) -> String {
    let mut out = String::new();
    for item in items {
        match item {
            Item::Fn(def) => {
                out.push_str("function ");
                out.push_str(&def.name);
                out.push_str(&params_str(&def.params));
                out.push_str(" {\n");
                for stmt in &def.body {
                    stmt_str(&mut out, stmt, 1);
                }
                out.push_str("}\n");
            }
            Item::Stmt(stmt) => stmt_str(&mut out, stmt, 0),
        }
    }
    out
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn params_str(params: &[String]) -> String {
    format!("({})", params.join(", "))
}

fn stmt_str(out: &mut String, stmt: &Stmt, level: usize) {
    indent(out, level);
    match stmt {
        Stmt::Empty => {}
        Stmt::Let {
            is_const, decls, ..
        } => {
            out.push_str(if *is_const { "const " } else { "let " });
            for (i, (name, init)) in decls.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(name);
                if let Some(init) = init {
                    out.push_str(" = ");
                    expr_str(out, init);
                }
            }
            out.push_str(";\n");
        }
        Stmt::Expr(expr) => {
            expr_str(out, expr);
            out.push_str(";\n");
        }
        Stmt::If(cond, then, els) => {
            out.push_str("if (");
            expr_str(out, cond);
            out.push_str(") ");
            stmt_as_block(out, then, level);
            if let Some(els) = els {
                out.push_str(" else ");
                stmt_as_block(out, els, level);
            }
            out.push('\n');
        }
        Stmt::While(cond, body) => {
            out.push_str("while (");
            expr_str(out, cond);
            out.push_str(") ");
            stmt_as_block(out, body, level);
            out.push('\n');
        }
        Stmt::For {
            init,
            cond,
            step,
            body,
        } => {
            out.push_str("for (");
            match init {
                Some(s) => stmt_inline(out, s),
                None => out.push(';'),
            }
            out.push(' ');
            if let Some(c) = cond {
                expr_str(out, c);
            }
            out.push_str("; ");
            if let Some(s) = step {
                expr_str(out, s);
            }
            out.push_str(") ");
            stmt_as_block(out, body, level);
            out.push('\n');
        }
        Stmt::ForOf {
            name,
            is_const,
            iterable,
            body,
        } => {
            out.push_str("for (");
            out.push_str(if *is_const { "const " } else { "let " });
            out.push_str(name);
            out.push_str(" of ");
            expr_str(out, iterable);
            out.push_str(") ");
            stmt_as_block(out, body, level);
            out.push('\n');
        }
        Stmt::Var { decls, .. } => {
            out.push_str("var ");
            for (i, (name, init)) in decls.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(name);
                if let Some(init) = init {
                    out.push_str(" = ");
                    expr_str(out, init);
                }
            }
            out.push_str(";\n");
        }
        Stmt::ForIn {
            name,
            is_const,
            iterable,
            body,
        } => {
            out.push_str("for (");
            out.push_str(if *is_const { "const " } else { "let " });
            out.push_str(name);
            out.push_str(" in ");
            expr_str(out, iterable);
            out.push_str(") ");
            stmt_as_block(out, body, level);
            out.push('\n');
        }
        Stmt::Block(stmts) => {
            block_str(out, stmts, level);
            out.push('\n');
        }
        Stmt::Return(expr) => {
            if let Some(expr) = expr {
                out.push_str("return ");
                expr_str(out, expr);
                out.push_str(";\n");
            } else {
                out.push_str("return;\n");
            }
        }
        Stmt::Break => out.push_str("break;\n"),
        Stmt::Continue => out.push_str("continue;\n"),
        Stmt::FnDecl(name, params, body) => {
            out.push_str("function ");
            out.push_str(name);
            out.push_str(&params_str(params));
            out.push_str(" {\n");
            for stmt in body {
                stmt_str(out, stmt, level + 1);
            }
            indent(out, level);
            out.push_str("}\n");
        }
    }
}

/// Statements that head a control-flow construct print as blocks so the
/// output never depends on single-statement grammar.
fn stmt_as_block(out: &mut String, stmt: &Stmt, level: usize) {
    match stmt {
        Stmt::Block(stmts) => block_str(out, stmts, level),
        single => {
            out.push_str("{\n");
            stmt_str(out, single, level + 1);
            indent(out, level);
            out.push('}');
        }
    }
}

fn stmt_inline(out: &mut String, stmt: &Stmt) {
    match stmt {
        Stmt::Let {
            is_const, decls, ..
        } => {
            out.push_str(if *is_const { "const " } else { "let " });
            for (i, (name, init)) in decls.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(name);
                if let Some(init) = init {
                    out.push_str(" = ");
                    expr_str(out, init);
                }
            }
            out.push(';');
        }
        Stmt::Expr(expr) => {
            expr_str(out, expr);
            out.push(';');
        }
        other => stmt_str(out, other, 0),
    }
}

fn block_str(out: &mut String, stmts: &[Stmt], level: usize) {
    out.push_str("{\n");
    for stmt in stmts {
        stmt_str(out, stmt, level + 1);
    }
    indent(out, level);
    out.push('}');
}

fn expr_str(out: &mut String, expr: &Expr) {
    match expr {
        Expr::Num(n) => out.push_str(&crate::value::fmt_number(*n)),
        Expr::Str(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    '\r' => out.push_str("\\r"),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        Expr::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Expr::Null => out.push_str("null"),
        Expr::Undefined => out.push_str("undefined"),
        Expr::Ident(name) => out.push_str(name),
        Expr::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                expr_str(out, item);
            }
            out.push(']');
        }
        Expr::Unary(op, e) => {
            out.push_str(match op {
                UnaryOp::Not => "!",
                UnaryOp::Neg => "-",
                UnaryOp::Typeof => "typeof ",
            });
            wrap(out, e, 9, expr);
        }
        Expr::Binary(op, l, r) => {
            let prec = bin_prec(op);
            wrap(out, l, prec, expr);
            out.push(' ');
            out.push_str(bin_str(op));
            out.push(' ');
            wrap(out, r, prec + 1, expr);
        }
        Expr::Logical(op, l, r) => {
            wrap(out, l, 2, expr);
            out.push_str(if *op == LogicalOp::And {
                " && "
            } else {
                " || "
            });
            wrap(out, r, 3, expr);
        }
        Expr::Eq(op, l, r) => {
            wrap(out, l, 4, expr);
            out.push_str(match op {
                EqOp::Strict => " === ",
                EqOp::StrictNe => " !== ",
                EqOp::Loose => " == ",
                EqOp::LooseNe => " != ",
            });
            wrap(out, r, 5, expr);
        }
        Expr::Assign(target, value) => {
            target_str(out, target);
            out.push_str(" = ");
            expr_str(out, value);
        }
        Expr::Index(obj, idx) => {
            wrap(out, obj, 17, expr);
            out.push('[');
            expr_str(out, idx);
            out.push(']');
        }
        Expr::Member(obj, prop) => {
            wrap(out, obj, 17, expr);
            out.push('.');
            out.push_str(prop);
        }
        Expr::Call(callee, args) => {
            wrap(out, callee, 17, expr);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                expr_str(out, a);
            }
            out.push(')');
        }
        Expr::Obj(entries) => {
            out.push('{');
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&codegen_key(&entry.key));
                out.push_str(": ");
                expr_str(out, &entry.value);
            }
            out.push('}');
        }
        Expr::Ternary(cond, then, els) => {
            out.push('(');
            expr_str(out, cond);
            out.push_str(" ? ");
            expr_str(out, then);
            out.push_str(" : ");
            expr_str(out, els);
            out.push(')');
        }
        Expr::Arrow(params, body) => {
            out.push_str(&params_str(params));
            out.push_str(" => ");
            match &**body {
                FnBody::Expr(e) => expr_str(out, e),
                FnBody::Block(stmts) => block_str(out, stmts, 0),
            }
        }
        Expr::Fn(name, params, body) => {
            out.push_str("function ");
            if let Some(name) = name {
                out.push_str(name);
            }
            out.push_str(&params_str(params));
            out.push_str(" {\n");
            for stmt in body {
                stmt_str(out, stmt, 1);
            }
            out.push('}');
        }
        Expr::Update(op, prefix, target) => {
            let s = match op {
                UpdateOp::Inc => "++",
                UpdateOp::Dec => "--",
            };
            if *prefix {
                out.push_str(s);
                target_str(out, target);
            } else {
                target_str(out, target);
                out.push_str(s);
            }
        }
    }
}

/// Object keys print bare when they are valid identifiers or plain
/// digit runs; anything else prints as a quoted string (a key like
/// `with space` is invalid JavaScript unquoted).
fn codegen_key(key: &str) -> String {
    let ident_shaped = !key.is_empty()
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    let numeric = !key.is_empty() && key.chars().all(|c| c.is_ascii_digit());
    if ident_shaped || numeric {
        key.to_string()
    } else {
        let mut out = String::with_capacity(key.len() + 2);
        out.push('"');
        for c in key.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
}

fn target_str(out: &mut String, target: &Target) {
    match target {
        Target::Ident(name) => out.push_str(name),
        Target::Index(obj, idx) => {
            expr_str(out, obj);
            out.push('[');
            expr_str(out, idx);
            out.push(']');
        }
        Target::Member(obj, prop) => {
            expr_str(out, obj);
            out.push('.');
            out.push_str(prop);
        }
    }
}

/// Wraps `e` in parens when its precedence is lower than `min`.
fn wrap(out: &mut String, e: &Expr, min: u8, _parent: &Expr) {
    let needs = expr_prec(e).map(|p| p < min).unwrap_or(true);
    if needs {
        out.push('(');
        expr_str(out, e);
        out.push(')');
    } else {
        expr_str(out, e);
    }
}

/// Precedence for printing; `None` for nodes that always want parens
/// when nested (arrows, assignments, ternaries).
fn expr_prec(e: &Expr) -> Option<u8> {
    match e {
        Expr::Num(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Ident(_)
        | Expr::Array(_)
        | Expr::Obj(_) => Some(20),
        Expr::Index(..) | Expr::Member(..) | Expr::Call(..) => Some(17),
        Expr::Unary(..) => Some(9),
        Expr::Binary(op, ..) => Some(bin_prec(op)),
        Expr::Logical(..) => Some(2),
        Expr::Eq(..) => Some(4),
        Expr::Ternary(..) | Expr::Assign(..) | Expr::Arrow(..) | Expr::Fn(..) => None,
        Expr::Update(..) => Some(16),
    }
}

fn bin_prec(op: &BinOp) -> u8 {
    match op {
        BinOp::Add | BinOp::Sub => 6,
        BinOp::Mul | BinOp::Div | BinOp::Rem => 7,
        BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => 5,
    }
}

fn bin_str(op: &BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::Le => "<=",
        BinOp::Ge => ">=",
    }
}
