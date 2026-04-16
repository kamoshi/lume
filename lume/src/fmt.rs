//! Format a Lume AST back to source code.
//!
//! Uses `crate::pretty` (Wadler-Lindig) to lay out the output.
//! Call `format_program(program)` to get a formatted `String`.

use crate::ast::*;
use crate::pretty::{
    concat, concat_all, group, hardline, join, line, nest, nil, render, space, text, wrap, Doc,
};

const WIDTH: usize = 80;
const INDENT: usize = 2;

// ── Public entry point ─────────────────────────────────────────────────────────

pub fn format_program(program: &Program) -> String {
    let doc = fmt_program(program);
    let mut out = render(doc, WIDTH);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

// ── Program ────────────────────────────────────────────────────────────────────

fn fmt_program(program: &Program) -> Doc {
    let mut parts: Vec<Doc> = Vec::new();

    for u in &program.uses {
        parts.push(fmt_use(u));
    }

    if !program.uses.is_empty() && !program.items.is_empty() {
        parts.push(hardline());
    }

    for (i, item) in program.items.iter().enumerate() {
        if i > 0 {
            parts.push(hardline());
        }
        parts.push(fmt_top_item(item));
    }

    // Only emit `pub { ... }` when there are actual exports.
    if let ExprKind::Record { fields, .. } = &program.exports.kind {
        if !fields.is_empty() {
            if !program.items.is_empty() || !program.uses.is_empty() {
                parts.push(hardline());
            }
            parts.push(fmt_pub_exports(&program.exports));
        }
    }

    concat_all(parts)
}

fn fmt_use(u: &UseDecl) -> Doc {
    let binding = match &u.binding {
        UseBinding::Ident(name, _, _) => text(name.clone()),
        UseBinding::Record(rp) => fmt_record_pattern(rp),
    };
    concat_all(vec![
        text("use "),
        binding,
        text(" = "),
        text(format!("\"{}\"", u.path)),
    ])
}

fn fmt_pub_exports(expr: &Expr) -> Doc {
    concat(text("pub "), fmt_expr(expr, 0))
}

// ── Top-level items ────────────────────────────────────────────────────────────

fn fmt_top_item(item: &TopItem) -> Doc {
    match item {
        TopItem::TypeDef(td) => fmt_typedef(td),
        TopItem::Binding(b) => fmt_binding(b),
        TopItem::BindingGroup(bs) => {
            let mut parts: Vec<Doc> = Vec::new();
            for (i, b) in bs.iter().enumerate() {
                if i > 0 {
                    parts.push(hardline());
                    parts.push(text("and "));
                }
                parts.push(fmt_binding(b));
            }
            concat_all(parts)
        }
        // Trait/impl defs are not yet formatted — emit nothing.
        TopItem::TraitDef(_) | TopItem::ImplDef(_) => nil(),
    }
}

// ── Type definitions ───────────────────────────────────────────────────────────

fn fmt_typedef(td: &TypeDef) -> Doc {
    let header = if td.params.is_empty() {
        format!("type {} =", td.name)
    } else {
        format!("type {} {} =", td.name, td.params.join(" "))
    };

    // flat: "type Foo = | A | B | C"
    // break: "type Foo =\n  | A\n  | B"
    let variant_docs: Vec<Doc> = td.variants.iter().map(fmt_variant).collect();
    let variants_flat = join(space(), variant_docs.clone());
    let _ = variants_flat;
    concat(
        text(header),
        group(concat(
            space(),
            join(
                line(),
                td.variants.iter().map(fmt_variant).collect::<Vec<_>>(),
            ),
        )),
    )
}

fn fmt_variant(v: &Variant) -> Doc {
    match &v.payload {
        None => text(format!("| {}", v.name)),
        Some(rt) => concat_all(vec![text(format!("| {} ", v.name)), fmt_record_type(rt)]),
    }
}

// ── Bindings ───────────────────────────────────────────────────────────────────

fn fmt_binding(b: &Binding) -> Doc {
    let pat = fmt_pattern(&b.pattern);
    let ty_ann = match &b.ty {
        None => nil(),
        Some(ty) => concat(text(" : "), fmt_type(ty)),
    };
    let rhs = fmt_expr_binding_rhs(&b.value);
    concat_all(vec![text("let "), pat, ty_ann, text(" ="), rhs])
}

/// Format the right-hand side of a binding.
/// Tries `let x = value` on one line; breaks to `let x =\n  value` if too long.
fn fmt_expr_binding_rhs(expr: &Expr) -> Doc {
    let val = fmt_expr(expr, 0);
    group(nest(INDENT, concat(line(), val)))
}

// ── Expressions ────────────────────────────────────────────────────────────────

/// Context precedence - wrap in parens if the expression's own precedence is lower.
fn expr_prec(expr: &Expr) -> u8 {
    match &expr.kind {
        ExprKind::Lambda { .. } => 0,
        ExprKind::LetIn { .. } => 1,
        ExprKind::If { .. } => 1,
        ExprKind::Match(_) => 1,
        ExprKind::MatchExpr { .. } => 1,
        ExprKind::Binary { op, .. } => binop_prec(op),
        ExprKind::Unary { .. } => 70,
        ExprKind::Apply { .. } => 60,
        _ => 100,
    }
}

fn binop_prec(op: &BinOp) -> u8 {
    match op {
        BinOp::Pipe | BinOp::ResultPipe => 10,
        BinOp::Or => 20,
        BinOp::And => 30,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => 40,
        BinOp::Add | BinOp::Sub => 50,
        BinOp::Mul | BinOp::Div => 55,
        BinOp::Concat => 45,
    }
}

fn binop_str(op: &BinOp) -> &'static str {
    match op {
        BinOp::Pipe => "|>",
        BinOp::ResultPipe => "?>",
        BinOp::Or => "||",
        BinOp::And => "&&",
        BinOp::Eq => "==",
        BinOp::NotEq => "!=",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::LtEq => "<=",
        BinOp::GtEq => ">=",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Concat => "++",
    }
}

fn fmt_expr(expr: &Expr, ctx_prec: u8) -> Doc {
    let my_prec = expr_prec(expr);
    let inner = fmt_expr_inner(expr);
    if my_prec < ctx_prec {
        wrap("(", ")", inner)
    } else {
        inner
    }
}

fn fmt_expr_inner(expr: &Expr) -> Doc {
    match &expr.kind {
        ExprKind::Number(n) => {
            // Print integers without decimal point.
            if n.fract() == 0.0 && n.abs() < 1e15 {
                text(format!("{}", *n as i64))
            } else {
                text(format!("{}", n))
            }
        }
        ExprKind::Text(s) => text(format!("\"{}\"", escape_string(s))),
        ExprKind::Bool(b) => text(if *b { "true" } else { "false" }),
        ExprKind::Ident(name) => text(name.clone()),

        ExprKind::List(elems) => {
            if elems.is_empty() {
                return text("[]");
            }
            let items: Vec<Doc> = elems.iter().map(|e| fmt_expr(e, 0)).collect();
            let inner = join(concat(text(","), line()), items);
            group(wrap(
                "[",
                "]",
                concat(nest(INDENT, concat(line(), inner)), line()),
            ))
        }

        ExprKind::Record { base, fields, .. } => fmt_record_expr(base.as_deref(), fields),

        ExprKind::FieldAccess { record, field } => {
            concat(fmt_expr(record, 100), text(format!(".{}", field)))
        }

        ExprKind::Variant {
            name,
            payload: None,
        } => text(name.clone()),
        ExprKind::Variant {
            name,
            payload: Some(p),
        } => concat(text(format!("{} ", name)), fmt_expr(p, 100)),

        ExprKind::Lambda { .. } => {
            // Peel nested lambdas: a -> b -> c -> body
            let mut params: Vec<&Pattern> = Vec::new();
            let mut cur = expr;
            loop {
                match &cur.kind {
                    ExprKind::Lambda { param, body } => {
                        params.push(param);
                        cur = body;
                    }
                    _ => break,
                }
            }
            let params_doc = join(
                text(" -> "),
                params.iter().map(|p| fmt_pattern(p)).collect::<Vec<_>>(),
            );
            concat(params_doc, concat(text(" -> "), fmt_expr(cur, 0)))
        }

        ExprKind::Apply { .. } => {
            // Collect the full application spine: f a b c
            let (func, args) = collect_apply(expr);
            let func_doc = fmt_expr(func, 60);
            let args_doc = join(
                line(),
                args.iter().map(|a| fmt_expr(a, 61)).collect::<Vec<_>>(),
            );
            group(concat(func_doc, nest(INDENT, concat(line(), args_doc))))
        }

        ExprKind::Binary { op, left, right } => {
            let prec = binop_prec(op);
            let op_s = binop_str(op);
            // Right-associative: right side uses same prec; left side uses prec+1
            let l = fmt_expr(left, prec + 1);
            let r = fmt_expr(right, prec);
            group(concat_all(vec![
                l,
                concat(line(), text(format!("{} ", op_s))),
                r,
            ]))
        }

        ExprKind::Unary { op, operand } => {
            let op_s = match op {
                UnOp::Neg => "-",
                UnOp::Not => "not ",
            };
            concat(text(op_s), fmt_expr(operand, 70))
        }

        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let cond_doc = fmt_expr(cond, 0);
            let then_doc = fmt_expr(then_branch, 0);
            let else_doc = fmt_expr(else_branch, 0);
            group(concat_all(vec![
                text("if "),
                cond_doc,
                nest(INDENT, concat(line(), concat(text("then "), then_doc))),
                nest(INDENT, concat(line(), concat(text("else "), else_doc))),
            ]))
        }

        ExprKind::Match(arms) => {
            let arms_doc: Vec<Doc> = arms.iter().map(fmt_match_arm).collect();
            concat_all(arms_doc)
        }

        ExprKind::MatchExpr { scrutinee, arms } => {
            let scrut_doc = fmt_expr(scrutinee, 0);
            let arms_doc: Vec<Doc> = arms.iter().map(fmt_match_arm).collect();
            concat_all(vec![
                text("match "),
                scrut_doc,
                text(" in"),
                nest(INDENT, concat(line(), concat_all(arms_doc))),
            ])
        }

        ExprKind::LetIn {
            pattern,
            value,
            body,
        } => {
            let pat_doc = fmt_pattern(pattern);
            let val_doc = fmt_expr(value, 0);
            let body_doc = fmt_expr(body, 0);
            concat_all(vec![
                text("let "),
                pat_doc,
                text(" = "),
                val_doc,
                text(" in"),
                nest(INDENT, concat(line(), body_doc)),
            ])
        }

        ExprKind::TraitCall {
            trait_name,
            method_name,
        } => text(format!("{}.{}", trait_name, method_name)),
    }
}

fn fmt_match_arm(arm: &MatchArm) -> Doc {
    let pat_doc = fmt_pattern(&arm.pattern);
    let guard_doc = match &arm.guard {
        None => nil(),
        Some(g) => concat(text(" if "), fmt_expr(g, 0)),
    };
    let body_doc = fmt_expr(&arm.body, 0);
    concat_all(vec![
        text("| "),
        pat_doc,
        guard_doc,
        text(" -> "),
        body_doc,
        hardline(),
    ])
}

fn collect_apply(expr: &Expr) -> (&Expr, Vec<&Expr>) {
    let mut args: Vec<&Expr> = Vec::new();
    let mut cur = expr;
    loop {
        match &cur.kind {
            ExprKind::Apply { func, arg } => {
                args.push(arg);
                cur = func;
            }
            _ => break,
        }
    }
    args.reverse();
    (cur, args)
}

fn escape_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ── Records ────────────────────────────────────────────────────────────────────

fn fmt_record_expr(base: Option<&Expr>, fields: &[RecordField]) -> Doc {
    if fields.is_empty() && base.is_none() {
        return text("{}");
    }
    let base_doc = match base {
        None => nil(),
        Some(b) => concat(fmt_expr(b, 0), text(" | ")),
    };
    let field_docs: Vec<Doc> = fields
        .iter()
        .map(|f| {
            let val = match &f.value {
                None => nil(), // shorthand { age }
                Some(v) => concat(text(": "), fmt_expr(v, 0)),
            };
            concat(text(f.name.clone()), val)
        })
        .collect();
    let inner = concat(base_doc, join(concat(text(","), line()), field_docs));
    group(wrap(
        "{ ",
        " }",
        nest(INDENT, concat(nil(), concat(inner, nil()))),
    ))
}

// ── Patterns ──────────────────────────────────────────────────────────────────

fn fmt_pattern(pat: &Pattern) -> Doc {
    match pat {
        Pattern::Wildcard => text("_"),
        Pattern::Literal(lit) => match lit {
            Literal::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    text(format!("{}", *n as i64))
                } else {
                    text(format!("{}", n))
                }
            }
            Literal::Text(s) => text(format!("\"{}\"", escape_string(s))),
            Literal::Bool(b) => text(if *b { "true" } else { "false" }),
        },
        Pattern::Ident(name, _, _) => text(name.clone()),
        Pattern::Variant {
            name,
            payload: None,
        } => text(name.clone()),
        Pattern::Variant {
            name,
            payload: Some(p),
        } => concat(text(format!("{} ", name)), fmt_pattern(p)),
        Pattern::Record(rp) => fmt_record_pattern(rp),
        Pattern::List(lp) => fmt_list_pattern(lp),
    }
}

fn fmt_record_pattern(rp: &RecordPattern) -> Doc {
    let mut parts: Vec<Doc> = rp
        .fields
        .iter()
        .map(|f| {
            let sub = match &f.pattern {
                None => nil(), // shorthand
                Some(p) => concat(text(": "), fmt_pattern(p)),
            };
            concat(text(f.name.clone()), sub)
        })
        .collect();
    if let Some(rest) = &rp.rest {
        let rest_doc = match rest {
            None => text(".."),
            Some(name) => text(format!("..{}", name)),
        };
        parts.push(rest_doc);
    }
    let inner = join(concat(text(","), line()), parts);
    group(wrap("{ ", " }", inner))
}

fn fmt_list_pattern(lp: &ListPattern) -> Doc {
    let mut parts: Vec<Doc> = lp.elements.iter().map(fmt_pattern).collect();
    if let Some(rest) = &lp.rest {
        let rest_doc = match rest {
            None => text(".."),
            Some(name) => text(format!("..{}", name)),
        };
        parts.push(rest_doc);
    }
    if parts.is_empty() {
        return text("[]");
    }
    let inner = join(concat(text(","), line()), parts);
    group(wrap("[", "]", inner))
}

// ── Types ──────────────────────────────────────────────────────────────────────

fn fmt_type(ty: &Type) -> Doc {
    fmt_type_prec(ty, 0)
}

fn fmt_type_prec(ty: &Type, ctx: u8) -> Doc {
    match ty {
        Type::Var(name) => text(name.clone()),
        Type::Named { name, args } => {
            if args.is_empty() {
                text(name.clone())
            } else {
                let args_doc = join(
                    space(),
                    args.iter()
                        .map(|a| fmt_type_prec(a, 10))
                        .collect::<Vec<_>>(),
                );
                let d = concat(text(format!("{} ", name)), args_doc);
                if ctx >= 10 {
                    wrap("(", ")", d)
                } else {
                    d
                }
            }
        }
        Type::Record(rt) => fmt_record_type(rt),
        Type::Func { param, ret } => {
            let d = concat_all(vec![
                fmt_type_prec(param, 1),
                text(" -> "),
                fmt_type_prec(ret, 0),
            ]);
            if ctx >= 1 {
                wrap("(", ")", d)
            } else {
                d
            }
        }
    }
}

fn fmt_record_type(rt: &RecordType) -> Doc {
    let mut parts: Vec<Doc> = rt
        .fields
        .iter()
        .map(|f| concat(text(format!("{}: ", f.name)), fmt_type(&f.ty)))
        .collect();
    if rt.open {
        parts.push(text(".."));
    }
    if parts.is_empty() {
        return text("{}");
    }
    let inner = join(concat(text(","), line()), parts);
    group(wrap("{ ", " }", inner))
}
