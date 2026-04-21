//! Dict-application hoisting pass.
//!
//! After lowering, parameterised impl dictionaries appear as function
//! applications at every call site — e.g. `__show_List(__show_Num)` wherever
//! the `Show (List Num)` dictionary is needed.  Each occurrence builds a fresh
//! record object at runtime, even though the result is always the same value.
//!
//! This pass walks each `ir::Module` and replaces every *static dict
//! application chain* with a reference to a single top-level `let` binding
//! injected just before its first use site.  Identical applications share one
//! binding, and nested chains are handled correctly bottom-up.
//!
//! ## Static dict chain
//!
//! A *static dict chain* is defined inductively:
//!
//! * `Var(name)` where `name` is an impl-dict variable — starts with `__` but
//!   NOT the `__dict_TraitName_N` lambda-parameter convention (those are
//!   runtime values, not globals).
//! * `Field(Var(mod), name)` where `name` is an impl-dict name — a
//!   cross-module impl-dict reference.
//! * `App(chain, chain)` — a parameterised dict applied to a concrete dict.
//! * `Var("__dict_inst_N")` — a name produced by a previous invocation of
//!   this pass (treated as static so nested hoisting composes correctly).
//!
//! Only the `App` form is actually hoisted; bare `Var`/`Field` references are
//! already cheap globals and need no transformation.

use std::collections::HashMap;

use crate::ir;

/// Run the dict-application hoisting optimisation on one IR module.
pub fn hoist_dict_applications(module: ir::Module) -> ir::Module {
    let mut ctx = HoistCtx::default();

    let mut new_items: Vec<ir::Decl> = Vec::new();
    for decl in module.items {
        let prev = ctx.bindings.len();
        let rewritten = ctx.rewrite_decl(decl);
        // Emit any bindings discovered while rewriting this declaration
        // immediately before it, so their dependencies are always in scope.
        for (name, expr) in ctx.bindings[prev..].iter() {
            new_items.push(ir::Decl::Let(ir::Pat::Var(name.clone()), expr.clone()));
        }
        new_items.push(rewritten);
    }

    // Rewrite the module exports; any newly-discovered bindings go last.
    let prev = ctx.bindings.len();
    let exports = ctx.rewrite_expr(module.exports);
    for (name, expr) in ctx.bindings[prev..].iter() {
        new_items.push(ir::Decl::Let(ir::Pat::Var(name.clone()), expr.clone()));
    }

    ir::Module {
        imports: module.imports,
        items: new_items,
        exports,
    }
}

// ── Internals ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct HoistCtx {
    /// Canonical key → hoisted binding name (for deduplication).
    intern: HashMap<String, String>,
    /// New bindings in discovery order (dependency order is guaranteed
    /// because we process bottom-up: inner chains are always interned before
    /// the outer chain that wraps them).
    bindings: Vec<(String, ir::Expr)>,
    counter: usize,
}

impl HoistCtx {
    fn rewrite_expr(&mut self, expr: ir::Expr) -> ir::Expr {
        match expr {
            ir::Expr::App(f, arg) => {
                // Bottom-up: rewrite children first so nested chains are
                // hoisted before we evaluate the outer chain.
                let f2 = self.rewrite_expr(*f);
                let arg2 = self.rewrite_expr(*arg);
                let app = ir::Expr::App(Box::new(f2), Box::new(arg2));
                if is_dict_app_chain(&app) {
                    self.intern_chain(app)
                } else {
                    app
                }
            }
            ir::Expr::Lam(pat, body) => {
                ir::Expr::Lam(pat, Box::new(self.rewrite_expr(*body)))
            }
            ir::Expr::Let(pat, val, body) => ir::Expr::Let(
                pat,
                Box::new(self.rewrite_expr(*val)),
                Box::new(self.rewrite_expr(*body)),
            ),
            ir::Expr::If(cond, then_, else_) => ir::Expr::If(
                Box::new(self.rewrite_expr(*cond)),
                Box::new(self.rewrite_expr(*then_)),
                Box::new(self.rewrite_expr(*else_)),
            ),
            ir::Expr::Match(scrut, arms) => ir::Expr::Match(
                Box::new(self.rewrite_expr(*scrut)),
                arms.into_iter().map(|a| self.rewrite_branch(a)).collect(),
            ),
            ir::Expr::MatchFn(arms) => {
                ir::Expr::MatchFn(arms.into_iter().map(|a| self.rewrite_branch(a)).collect())
            }
            ir::Expr::Record { bases, fields } => ir::Expr::Record {
                bases: bases.into_iter().map(|b| self.rewrite_expr(b)).collect(),
                fields: fields.into_iter().map(|(k, v)| (k, self.rewrite_expr(v))).collect(),
            },
            ir::Expr::Field(rec, name) => {
                ir::Expr::Field(Box::new(self.rewrite_expr(*rec)), name)
            }
            ir::Expr::List { bases, elems } => ir::Expr::List {
                bases: bases.into_iter().map(|b| self.rewrite_expr(b)).collect(),
                elems: elems.into_iter().map(|e| self.rewrite_expr(e)).collect(),
            },
            ir::Expr::Tag(name, payload) => {
                ir::Expr::Tag(name, payload.map(|p| Box::new(self.rewrite_expr(*p))))
            }
            ir::Expr::BinOp(op, l, r) => ir::Expr::BinOp(
                op,
                Box::new(self.rewrite_expr(*l)),
                Box::new(self.rewrite_expr(*r)),
            ),
            ir::Expr::UnOp(op, e) => ir::Expr::UnOp(op, Box::new(self.rewrite_expr(*e))),
            // Leaves — nothing to recurse into.
            leaf => leaf,
        }
    }

    fn rewrite_branch(&mut self, branch: ir::Branch) -> ir::Branch {
        ir::Branch {
            pattern: branch.pattern,
            guard: branch.guard.map(|g| self.rewrite_expr(g)),
            body: self.rewrite_expr(branch.body),
        }
    }

    fn rewrite_decl(&mut self, decl: ir::Decl) -> ir::Decl {
        match decl {
            ir::Decl::Let(pat, expr) => ir::Decl::Let(pat, self.rewrite_expr(expr)),
            ir::Decl::LetRec(bindings) => ir::Decl::LetRec(
                bindings.into_iter().map(|(p, e)| (p, self.rewrite_expr(e))).collect(),
            ),
        }
    }

    /// Intern a static dict application chain and return a `Var` reference to it.
    fn intern_chain(&mut self, expr: ir::Expr) -> ir::Expr {
        let key = chain_key(&expr);
        if let Some(name) = self.intern.get(&key) {
            return ir::Expr::Var(name.clone());
        }
        let name = format!("__dict_inst_{}", self.counter);
        self.counter += 1;
        self.intern.insert(key, name.clone());
        self.bindings.push((name.clone(), expr));
        ir::Expr::Var(name)
    }
}

// ── Predicate and key helpers ─────────────────────────────────────────────────

/// Returns `true` if `expr` is a static dict application chain that can be
/// safely hoisted to a top-level constant.
fn is_dict_app_chain(expr: &ir::Expr) -> bool {
    match expr {
        ir::Expr::Var(name) => is_impl_dict_name(name),
        ir::Expr::Field(base, name) => {
            is_impl_dict_name(name) && matches!(base.as_ref(), ir::Expr::Var(_))
        }
        ir::Expr::App(f, arg) => is_dict_app_chain(f) && is_dict_app_chain(arg),
        _ => false,
    }
}

/// True for names that refer to a global impl-dict variable.
///
/// Excludes `__dict_TraitName_N` names, which are lambda *parameters*
/// (runtime values) generated by the constrained-binding wrapper in lowering.
/// Includes `__dict_inst_N` names emitted by this pass itself so that
/// previously-hoisted bindings compose correctly with further hoisting.
fn is_impl_dict_name(name: &str) -> bool {
    if !name.starts_with("__") {
        return false;
    }
    if name.starts_with("__dict_") {
        // Only our own hoisted names are static; lambda params are not.
        return name.starts_with("__dict_inst_");
    }
    true
}

/// Compute a canonical string key for a static dict chain.
/// Used as the deduplication key in `HoistCtx::intern`.
fn chain_key(expr: &ir::Expr) -> String {
    match expr {
        ir::Expr::Var(name) => name.clone(),
        ir::Expr::Field(base, name) => {
            let ir::Expr::Var(mod_name) = base.as_ref() else {
                unreachable!("non-Var base in cross-module dict field ref")
            };
            format!("{}.{}", mod_name, name)
        }
        ir::Expr::App(f, arg) => format!("{}({})", chain_key(f), chain_key(arg)),
        _ => unreachable!("non-chain expression passed to chain_key"),
    }
}
