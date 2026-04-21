//! Eta-reduction pass.
//!
//! Converts `fn(x) -> f(x)` to `f` wherever `x` is not free in `f`.
//! This is the standard η-reduction rule from lambda calculus.
//!
//! ## Motivation
//!
//! After lowering, accessing a constrained binding `b : Foo a => a -> Text`
//! in a constrained context generates a new wrapper closure at every use site:
//!
//! ```text
//! _result = fn(__dict_Foo_0) -> b(__dict_Foo_0)
//! ```
//!
//! This is an eta-expansion of `b`. Reducing it back to `b` eliminates the
//! redundant wrapper and — in the REPL — stabilises function pointers for
//! constrained bindings, since `b` is the already-allocated global rather than
//! a freshly-allocated closure.

use std::collections::HashSet;

use crate::ir;

/// Run eta-reduction on every expression in an IR module.
pub fn eta_reduce(module: ir::Module) -> ir::Module {
    ir::Module {
        imports: module.imports,
        items: module.items.into_iter().map(reduce_decl).collect(),
        exports: reduce_expr(module.exports),
    }
}

// ── Recursive reducers ────────────────────────────────────────────────────────

fn reduce_decl(decl: ir::Decl) -> ir::Decl {
    match decl {
        ir::Decl::Let(pat, expr) => ir::Decl::Let(pat, reduce_expr(expr)),
        ir::Decl::LetRec(bindings) => {
            ir::Decl::LetRec(bindings.into_iter().map(|(p, e)| (p, reduce_expr(e))).collect())
        }
    }
}

fn reduce_expr(expr: ir::Expr) -> ir::Expr {
    match expr {
        ir::Expr::Lam(pat, body) => {
            // Bottom-up: reduce the body first, then try η at this node.
            let body = reduce_expr(*body);
            eta_step(pat, body)
        }
        ir::Expr::App(f, arg) => {
            ir::Expr::App(Box::new(reduce_expr(*f)), Box::new(reduce_expr(*arg)))
        }
        ir::Expr::Let(pat, val, body) => ir::Expr::Let(
            pat,
            Box::new(reduce_expr(*val)),
            Box::new(reduce_expr(*body)),
        ),
        ir::Expr::If(cond, then_, else_) => ir::Expr::If(
            Box::new(reduce_expr(*cond)),
            Box::new(reduce_expr(*then_)),
            Box::new(reduce_expr(*else_)),
        ),
        ir::Expr::Match(scrut, arms) => ir::Expr::Match(
            Box::new(reduce_expr(*scrut)),
            arms.into_iter().map(reduce_branch).collect(),
        ),
        ir::Expr::MatchFn(arms) => {
            ir::Expr::MatchFn(arms.into_iter().map(reduce_branch).collect())
        }
        ir::Expr::Record { bases, fields } => ir::Expr::Record {
            bases: bases.into_iter().map(reduce_expr).collect(),
            fields: fields.into_iter().map(|(k, v)| (k, reduce_expr(v))).collect(),
        },
        ir::Expr::Field(rec, name) => ir::Expr::Field(Box::new(reduce_expr(*rec)), name),
        ir::Expr::List { bases, elems } => ir::Expr::List {
            bases: bases.into_iter().map(reduce_expr).collect(),
            elems: elems.into_iter().map(reduce_expr).collect(),
        },
        ir::Expr::Tag(name, payload) => {
            ir::Expr::Tag(name, payload.map(|p| Box::new(reduce_expr(*p))))
        }
        ir::Expr::BinOp(op, l, r) => {
            ir::Expr::BinOp(op, Box::new(reduce_expr(*l)), Box::new(reduce_expr(*r)))
        }
        ir::Expr::UnOp(op, e) => ir::Expr::UnOp(op, Box::new(reduce_expr(*e))),
        leaf => leaf,
    }
}

fn reduce_branch(branch: ir::Branch) -> ir::Branch {
    ir::Branch {
        pattern: branch.pattern,
        guard: branch.guard.map(reduce_expr),
        body: reduce_expr(branch.body),
    }
}

/// Apply the η rule at a single `Lam` node whose body has already been reduced.
///
/// Rule: `fn(x) -> f(x)` → `f`  when `x ∉ free_vars(f)`
///
/// The function position `f` may be any expression (e.g. `App(g, a)`), so
/// multi-argument partial applications eta-reduce naturally.
fn eta_step(pat: ir::Pat, body: ir::Expr) -> ir::Expr {
    if let ir::Pat::Var(ref x) = pat {
        if let ir::Expr::App(ref f, ref arg) = body {
            if let ir::Expr::Var(ref y) = **arg {
                if x == y && !free_vars(f).contains(x.as_str()) {
                    return *f.clone();
                }
            }
        }
    }
    ir::Expr::Lam(pat, Box::new(body))
}

// ── Free-variable analysis ────────────────────────────────────────────────────

/// Returns the set of free variable names in `expr`.
fn free_vars(expr: &ir::Expr) -> HashSet<String> {
    let mut free = HashSet::new();
    collect_free(expr, &mut free, &HashSet::new());
    free
}

fn collect_free(expr: &ir::Expr, free: &mut HashSet<String>, bound: &HashSet<String>) {
    match expr {
        ir::Expr::Var(name) => {
            if !bound.contains(name.as_str()) {
                free.insert(name.clone());
            }
        }
        ir::Expr::Lam(pat, body) => {
            let b2 = extend_bound(bound, pat);
            collect_free(body, free, &b2);
        }
        ir::Expr::App(f, arg) => {
            collect_free(f, free, bound);
            collect_free(arg, free, bound);
        }
        ir::Expr::Let(pat, val, body) => {
            collect_free(val, free, bound);
            let b2 = extend_bound(bound, pat);
            collect_free(body, free, &b2);
        }
        ir::Expr::If(cond, then_, else_) => {
            collect_free(cond, free, bound);
            collect_free(then_, free, bound);
            collect_free(else_, free, bound);
        }
        ir::Expr::Match(scrut, arms) => {
            collect_free(scrut, free, bound);
            for arm in arms {
                collect_free_branch(arm, free, bound);
            }
        }
        ir::Expr::MatchFn(arms) => {
            for arm in arms {
                collect_free_branch(arm, free, bound);
            }
        }
        ir::Expr::Record { bases, fields } => {
            for b in bases {
                collect_free(b, free, bound);
            }
            for (_, v) in fields {
                collect_free(v, free, bound);
            }
        }
        ir::Expr::Field(rec, _) => collect_free(rec, free, bound),
        ir::Expr::List { bases, elems } => {
            for b in bases {
                collect_free(b, free, bound);
            }
            for e in elems {
                collect_free(e, free, bound);
            }
        }
        ir::Expr::Tag(_, payload) => {
            if let Some(p) = payload {
                collect_free(p, free, bound);
            }
        }
        ir::Expr::BinOp(_, l, r) => {
            collect_free(l, free, bound);
            collect_free(r, free, bound);
        }
        ir::Expr::UnOp(_, e) => collect_free(e, free, bound),
        ir::Expr::Num(_) | ir::Expr::Str(_) | ir::Expr::Bool(_) => {}
    }
}

fn collect_free_branch(branch: &ir::Branch, free: &mut HashSet<String>, bound: &HashSet<String>) {
    let b2 = extend_bound(bound, &branch.pattern);
    if let Some(guard) = &branch.guard {
        collect_free(guard, free, &b2);
    }
    collect_free(&branch.body, free, &b2);
}

/// Clone `bound` and add all names introduced by `pat`.
fn extend_bound(bound: &HashSet<String>, pat: &ir::Pat) -> HashSet<String> {
    let mut b = bound.clone();
    add_pat_names(pat, &mut b);
    b
}

fn add_pat_names(pat: &ir::Pat, bound: &mut HashSet<String>) {
    match pat {
        ir::Pat::Wild | ir::Pat::Lit(_) => {}
        ir::Pat::Var(name) => {
            bound.insert(name.clone());
        }
        ir::Pat::Tag(_, inner) => {
            if let Some(p) = inner {
                add_pat_names(p, bound);
            }
        }
        ir::Pat::Record { fields, rest } => {
            for (name, inner_pat) in fields {
                match inner_pat {
                    // `{ foo }` shorthand — binds the field name directly.
                    None => {
                        bound.insert(name.clone());
                    }
                    Some(p) => add_pat_names(p, bound),
                }
            }
            if let Some(Some(rest_name)) = rest {
                bound.insert(rest_name.clone());
            }
        }
        ir::Pat::List { elems, rest } => {
            for elem in elems {
                add_pat_names(elem, bound);
            }
            if let Some(Some(rest_name)) = rest {
                bound.insert(rest_name.clone());
            }
        }
    }
}
