//! Lowering pass: AST → IR.
//!
//! Transforms a typed `Program` (which may still contain `TraitDef`, `ImplDef`,
//! and `TraitCall` nodes) into an `ir::Module` that uses only core constructs.
//!
//! The transformation outputs a clean IR instead of mutating the AST in place:
//!
//! * `trait Show a { … }` → erased (type-checking only)
//! * `use Show in Num { show = expr }` → `Decl::Let("__show_Num", Record { show: expr })`
//! * `Show.show` (when `a ~ Num`) → `Field(Var("__show_Num"), "show")`
//! * `a |> f` → `App(f, a)`

use std::collections::HashMap;

use crate::ast::*;
use crate::ir;
use crate::types::{Ty, TyVar};
use crate::types::infer::{TypeEnv, VariantInfo};

// ── Public types ─────────────────────────────────────────────────────────────

/// One entry in the global impl table built from all bundle modules.
pub struct ImplEntry {
    pub module_var: Option<String>,
    pub dict_ident: String,
}

/// One entry in the parameterized impl table for constrained impls like
/// `use Show in Show a => List a { … }`.
pub struct ParamImplEntry {
    pub trait_name: String,
    pub target_type: Type,
    pub constraints: Vec<(String, String)>,
    pub module_var: Option<String>,
    pub dict_ident: String,
}

/// Global trait/impl context built once from the full bundle.
pub struct GlobalCtx {
    pub traits: HashMap<String, TraitDef>,
    pub impls: HashMap<(String, String), ImplEntry>,
    pub param_impls: Vec<ParamImplEntry>,
    pub variants: HashMap<String, VariantInfo>,
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Lower a typed `Program` into an `ir::Module`.
///
/// * `node_types`             — concrete types from `elaborate`, keyed by `NodeId`.
/// * `type_env`               — top-level type schemes (for detecting constrained bindings).
/// * `global`                 — trait/impl index from the whole bundle.
/// * `resolved_trait_methods` — `Ident` nodes resolved via unambiguous trait lookup.
pub fn lower(
    program: Program,
    node_types: &HashMap<NodeId, Ty>,
    type_env: &TypeEnv,
    global: &GlobalCtx,
    resolved_trait_methods: &HashMap<NodeId, (String, String)>,
    resolved_op_types: &HashMap<NodeId, Ty>,
    prelude_fields: &[String],
) -> ir::Module {
    let cx = Cx {
        node_types,
        type_env,
        global,
        dict_params: HashMap::new(),
        resolved_trait_methods,
        resolved_op_types,
    };

    // Collect impl dict names before consuming items — these must be exported.
    let impl_dict_names: Vec<String> = program
        .items
        .iter()
        .filter_map(|item| {
            if let TopItem::ImplDef(id) = item {
                Some(dict_name(&id.trait_name, &id.type_name))
            } else {
                None
            }
        })
        .collect();

    // Lower imports.
    let mut imports: Vec<ir::Import> = Vec::new();

    // Synthesize a prelude import that destructures all names the prelude
    // exports. The `prelude_fields` list is computed from the compiled prelude's
    // export record at the call site, so it stays in sync automatically.
    if !program.pragmas.no_prelude && !prelude_fields.is_empty() {
        imports.push(ir::Import {
            binding: ir::ImportBinding::Destructure(prelude_fields.to_vec()),
            path: "lume:prelude".into(),
        });
    }

    imports.extend(program.uses.iter().map(|u| ir::Import {
        binding: match &u.binding {
            UseBinding::Ident(name, _, _) => ir::ImportBinding::Name(name.clone()),
            UseBinding::Record(rp) => {
                ir::ImportBinding::Destructure(rp.fields.iter().map(|f| f.name.clone()).collect())
            }
        },
        path: u.path.clone(),
    }));

    // Lower items.
    let mut items = Vec::new();
    for item in program.items {
        match item {
            TopItem::TraitDef(_) | TopItem::TypeDef(_) => {}
            TopItem::ImplDef(id) => {
                let (pat, expr) = cx.lower_impl_def(id);
                items.push(ir::Decl::Let(pat, expr));
            }
            TopItem::Binding(b) => {
                let (pat, expr) = cx.lower_binding(b);
                items.push(ir::Decl::Let(pat, expr));
            }
            TopItem::BindingGroup(bs) => {
                let group: Vec<(ir::Pat, ir::Expr)> =
                    bs.into_iter().map(|b| cx.lower_binding(b)).collect();
                items.push(ir::Decl::LetRec(group));
            }
        }
    }

    // Augment exports with impl dicts.
    let mut exports = cx.expr(program.exports);
    if !impl_dict_names.is_empty() {
        if let ir::Expr::Record { ref mut fields, .. } = exports {
            for dict in impl_dict_names {
                fields.push((dict.clone(), ir::Expr::Var(dict)));
            }
        }
    }

    ir::Module { imports, items, exports }
}

// ── Internal context ─────────────────────────────────────────────────────────

struct Cx<'a> {
    node_types: &'a HashMap<NodeId, Ty>,
    type_env: &'a TypeEnv,
    global: &'a GlobalCtx,
    /// Inside a constrained binding: maps each TyVar to `(dict_param_name, trait_name)`.
    dict_params: HashMap<TyVar, (String, String)>,
    /// Ident nodes that the type checker resolved to an unambiguous trait method.
    resolved_trait_methods: &'a HashMap<NodeId, (String, String)>,
    /// Operator expressions resolved through traits — stores the operator's instantiated type.
    resolved_op_types: &'a HashMap<NodeId, Ty>,
}

impl<'a> Cx<'a> {
    fn with_dict_params(&self, dict_params: HashMap<TyVar, (String, String)>) -> Cx<'a> {
        Cx {
            node_types: self.node_types,
            type_env: self.type_env,
            global: self.global,
            dict_params,
            resolved_trait_methods: self.resolved_trait_methods,
            resolved_op_types: self.resolved_op_types,
        }
    }

    // ── Bindings ─────────────────────────────────────────────────────────

    /// Lower a binding, wrapping in dict-lambdas if its scheme has constraints.
    fn lower_binding(&self, b: Binding) -> (ir::Pat, ir::Expr) {
        let binding_name = match &b.pattern {
            Pattern::Ident(name, _, _) => Some(name.clone()),
            _ => None,
        };

        let scheme = binding_name
            .as_deref()
            .and_then(|name| self.type_env.lookup(name));

        let body = if let Some(scheme) = scheme {
            if !scheme.constraints.is_empty() {
                let mut dict_params: HashMap<TyVar, (String, String)> = HashMap::new();
                let mut dict_param_list: Vec<(String, TyVar, String)> = Vec::new();
                let mut counter: HashMap<String, usize> = HashMap::new();
                for (trait_name, var) in &scheme.constraints {
                    let idx = counter.entry(trait_name.clone()).or_insert(0);
                    let param_name = format!("__dict_{}_{}", trait_name, idx);
                    *idx += 1;
                    dict_params.insert(*var, (param_name.clone(), trait_name.clone()));
                    dict_param_list.push((param_name, *var, trait_name.clone()));
                }

                let child_cx = self.with_dict_params(dict_params);
                let mut body = child_cx.expr(b.value);

                // Wrap in dict-lambdas (innermost constraint last).
                for (param_name, _, _) in dict_param_list.iter().rev() {
                    body = ir::Expr::Lam(
                        ir::Pat::Var(param_name.clone()),
                        Box::new(body),
                    );
                }
                body
            } else {
                self.expr(b.value)
            }
        } else {
            self.expr(b.value)
        };

        (self.pat(b.pattern), body)
    }

    /// Lower an ImplDef to `(pat, expr)`: `__show_Num = { show: … }`.
    fn lower_impl_def(&self, id: ImplDef) -> (ir::Pat, ir::Expr) {
        let dict = dict_name(&id.trait_name, &id.type_name);

        // Check if the dict binding has a constrained scheme.
        let scheme = self.type_env.lookup(&dict);
        let has_constraints = scheme.as_ref().is_some_and(|s| !s.constraints.is_empty());

        if has_constraints {
            let scheme = scheme.unwrap();
            let mut dict_params_map: HashMap<TyVar, (String, String)> = HashMap::new();
            let mut dict_param_list: Vec<(String, TyVar, String)> = Vec::new();
            let mut counter: HashMap<String, usize> = HashMap::new();
            for (trait_name, var) in &scheme.constraints {
                let idx = counter.entry(trait_name.clone()).or_insert(0);
                let param_name = format!("__dict_{}_{}", trait_name, idx);
                *idx += 1;
                dict_params_map.insert(*var, (param_name.clone(), trait_name.clone()));
                dict_param_list.push((param_name, *var, trait_name.clone()));
            }

            // Lower method bodies with dict_params in scope so inner TraitCalls resolve.
            let child_cx = self.with_dict_params(dict_params_map);
            let fields: Vec<(String, ir::Expr)> = id
                .methods
                .into_iter()
                .map(|b| {
                    let name = match &b.pattern {
                        Pattern::Ident(n, _, _) => n.clone(),
                        _ => panic!("impl method patterns must be simple identifiers"),
                    };
                    (name, child_cx.expr(b.value))
                })
                .collect();

            let mut body = ir::Expr::Record { bases: vec![], fields };
            for (param_name, _, _) in dict_param_list.iter().rev() {
                body = ir::Expr::Lam(
                    ir::Pat::Var(param_name.clone()),
                    Box::new(body),
                );
            }
            (ir::Pat::Var(dict), body)
        } else {
            // No constraints — lower methods with the current context.
            let fields: Vec<(String, ir::Expr)> = id
                .methods
                .into_iter()
                .map(|b| {
                    let name = match &b.pattern {
                        Pattern::Ident(n, _, _) => n.clone(),
                        _ => panic!("impl method patterns must be simple identifiers"),
                    };
                    (name, self.expr(b.value))
                })
                .collect();
            (ir::Pat::Var(dict), ir::Expr::Record { bases: vec![], fields })
        }
    }

    // ── Expressions ──────────────────────────────────────────────────────

    fn expr(&self, e: Expr) -> ir::Expr {
        let id = e.id;
        match e.kind {
            ExprKind::TraitCall { trait_name, method_name } => {
                // Try dict_params first (polymorphic context).
                if let Some(ir) = self.resolve_trait_call_via_dict(id, &trait_name, &method_name) {
                    return ir;
                }
                // Try concrete type lookup.
                if let Some(ir) = self.resolve_trait_call(id, &trait_name, &method_name) {
                    return ir;
                }
                // Defensive: should be unreachable for well-typed programs
                // (check_trait_constraints catches missing impls).  Emit a
                // clear runtime error rather than a silent nil access.
                ir::Expr::App(
                    Box::new(ir::Expr::Var("__error".into())),
                    Box::new(ir::Expr::Str(
                        format!("unresolved trait method: {}.{}", trait_name, method_name),
                    )),
                )
            }

            ExprKind::Ident(ref name) => {
                // insert_dict_args returns None here for resolved trait methods
                // because they are never added to type_env — only to
                // resolved_trait_methods.
                if let Some(ir) = self.insert_dict_args(id, name) {
                    return ir;
                }
                // An Ident that the type checker resolved to a single unambiguous
                // trait method is lowered the same way as an explicit TraitCall.
                if let Some((trait_name, method_name)) = self.resolved_trait_methods.get(&id) {
                    if let Some(ir) = self.resolve_trait_call_via_dict(id, trait_name, method_name) {
                        return ir;
                    }
                    if let Some(ir) = self.resolve_trait_call(id, trait_name, method_name) {
                        return ir;
                    }
                    // Defensive: should be unreachable for well-typed programs
                    // (check_trait_constraints catches missing impls).  Emit a
                    // clear runtime error rather than a silent nil access.
                    return ir::Expr::App(
                        Box::new(ir::Expr::Var("__error".into())),
                        Box::new(ir::Expr::Str(
                            format!("unresolved trait method: {}.{}", trait_name, method_name),
                        )),
                    );
                }
                ir::Expr::Var(name.clone())
            }

            ExprKind::Number(n) => ir::Expr::Num(n),
            ExprKind::Text(s) => ir::Expr::Str(s),
            ExprKind::Bool(b) => ir::Expr::Bool(b),
            ExprKind::Hole => ir::Expr::Var("__hole".to_string()),

            ExprKind::List { entries } => {
                // Lower interleaved elems/spreads into IR List with bases.
                // Consecutive elements before a spread get flushed into a
                // nested List that becomes the next base.
                //
                // `[1, ..a, 2, 3, ..b]` lowers to:
                //   List { bases: [List{bases:[],elems:[1]}, a, List{bases:[],elems:[2,3]}, b],
                //          elems: [] }
                let mut bases: Vec<ir::Expr> = Vec::new();
                let mut pending_elems: Vec<ir::Expr> = Vec::new();

                for entry in entries {
                    match entry {
                        ListEntry::Elem(e) => {
                            pending_elems.push(self.expr(e));
                        }
                        ListEntry::Spread(e) => {
                            if !pending_elems.is_empty() {
                                bases.push(ir::Expr::List {
                                    bases: vec![],
                                    elems: std::mem::take(&mut pending_elems),
                                });
                            }
                            bases.push(self.expr(e));
                        }
                    }
                }

                ir::Expr::List { bases, elems: pending_elems }
            }

            ExprKind::Record { entries } => {
                // Lower interleaved fields/spreads into IR Record with bases.
                // Entries are applied left-to-right. Each spread becomes a base;
                // when a spread follows explicit fields, the fields are flushed
                // into a nested Record that becomes the next base.
                //
                // `{ ..a, x: 1, ..b, y: 2 }` lowers to:
                //   Record { bases: [a, Record { bases: [], fields: [(x,1)] }, b],
                //            fields: [(y, 2)] }
                let mut bases: Vec<ir::Expr> = Vec::new();
                let mut pending_fields: Vec<(String, ir::Expr)> = Vec::new();

                for entry in entries {
                    match entry {
                        RecordEntry::Field(f) => {
                            let val = match f.value {
                                Some(v) => self.expr(v),
                                None => ir::Expr::Var(f.name.clone()),
                            };
                            pending_fields.push((f.name, val));
                        }
                        RecordEntry::Spread(expr) => {
                            if !pending_fields.is_empty() {
                                bases.push(ir::Expr::Record {
                                    bases: vec![],
                                    fields: std::mem::take(&mut pending_fields),
                                });
                            }
                            bases.push(self.expr(expr));
                        }
                    }
                }

                ir::Expr::Record { bases, fields: pending_fields }
            }

            ExprKind::FieldAccess { record, field } => {
                ir::Expr::Field(Box::new(self.expr(*record)), field)
            }

            ExprKind::Variant { name, payload: None } => {
                // Bare constructor: if it wraps a type, desugar to lambda.
                if let Some(info) = self.global.variants.get(&name) {
                    if info.wraps.is_some() {
                        return ir::Expr::Lam(
                            ir::Pat::Var("__v".to_string()),
                            Box::new(ir::Expr::Tag(
                                name,
                                Some(Box::new(ir::Expr::Var("__v".to_string()))),
                            )),
                        );
                    }
                }
                ir::Expr::Tag(name, None)
            }

            ExprKind::Variant { name, payload: Some(payload) } => {
                ir::Expr::Tag(name, Some(Box::new(self.expr(*payload))))
            }

            ExprKind::Lambda { param, body } => {
                ir::Expr::Lam(self.pat(param), Box::new(self.expr(*body)))
            }

            ExprKind::Apply { func, arg } => {
                ir::Expr::App(Box::new(self.expr(*func)), Box::new(self.expr(*arg)))
            }

            ExprKind::Paren(inner) => self.expr(*inner),

            ExprKind::Binary { op: BinOp::Pipe, left, right } => {
                // a |> f  →  App(f, a)
                ir::Expr::App(Box::new(self.expr(*right)), Box::new(self.expr(*left)))
            }

            ExprKind::Binary { op: BinOp::Custom(name), left, right } => {
                // Check if the operator was resolved to a trait method.
                let op_expr = if let Some((trait_name, method_name)) = self.resolved_trait_methods.get(&id) {
                    // Use the operator's instantiated type for resolution.
                    let op_ty = self.resolved_op_types.get(&id);
                    if let Some(op_ty) = op_ty {
                        if let Some(ir) = self.resolve_trait_call_with_ty(trait_name, method_name, op_ty) {
                            ir
                        } else {
                            ir::Expr::Var(name)
                        }
                    } else {
                        ir::Expr::Var(name)
                    }
                } else {
                    ir::Expr::Var(name)
                };
                // a <op> b  →  App(App(op_expr, a), b)
                let app1 = ir::Expr::App(Box::new(op_expr), Box::new(self.expr(*left)));
                ir::Expr::App(Box::new(app1), Box::new(self.expr(*right)))
            }

            ExprKind::Binary { op, left, right } => {
                ir::Expr::BinOp(op, Box::new(self.expr(*left)), Box::new(self.expr(*right)))
            }

            ExprKind::Unary { op, operand } => {
                ir::Expr::UnOp(op, Box::new(self.expr(*operand)))
            }

            ExprKind::If { cond, then_branch, else_branch } => {
                ir::Expr::If(
                    Box::new(self.expr(*cond)),
                    Box::new(self.expr(*then_branch)),
                    Box::new(self.expr(*else_branch)),
                )
            }

            ExprKind::Match(arms) => {
                ir::Expr::MatchFn(arms.into_iter().map(|a| self.branch(a)).collect())
            }

            ExprKind::MatchExpr { scrutinee, arms } => {
                ir::Expr::Match(
                    Box::new(self.expr(*scrutinee)),
                    arms.into_iter().map(|a| self.branch(a)).collect(),
                )
            }

            ExprKind::LetIn { pattern, value, body } => {
                ir::Expr::Let(
                    self.pat(pattern),
                    Box::new(self.expr(*value)),
                    Box::new(self.expr(*body)),
                )
            }

            ExprKind::Sequence(exprs) => {
                let mut lowered: Vec<ir::Expr> = exprs.into_iter().map(|e| self.expr(e)).collect();
                let last = lowered.pop().expect("sequence is non-empty");
                // Build right-nested: let _ = e1 in let _ = e2 in ... en
                lowered.into_iter().rfold(last, |acc, e| {
                    ir::Expr::Let(ir::Pat::Wild, Box::new(e), Box::new(acc))
                })
            }

            ExprKind::Do { stmts, tail, .. } => self.lower_do(stmts, *tail),
        }
    }

    fn branch(&self, arm: MatchArm) -> ir::Branch {
        ir::Branch {
            pattern: self.pat(arm.pattern),
            guard: arm.guard.map(|g| self.expr(g)),
            body: self.expr(arm.body),
        }
    }

    /// Desugar a `do` block to nested `let`/`and_then` IR expressions.
    ///
    /// - `DoStmt::Let`  → `let pat = val in rest`
    /// - `DoStmt::Seq`  → `let _ = expr in rest`
    /// - `DoStmt::Bind` → `Monad.and_then val (pat -> rest)`
    fn lower_do(&self, stmts: Vec<DoStmt>, tail: Expr) -> ir::Expr {
        let mut iter = stmts.into_iter().peekable();
        self.lower_do_iter(&mut iter, tail)
    }

    fn lower_do_iter(
        &self,
        stmts: &mut std::iter::Peekable<std::vec::IntoIter<DoStmt>>,
        tail: Expr,
    ) -> ir::Expr {
        match stmts.next() {
            None => self.expr(tail),
            Some(DoStmt::Let { pattern, value }) => {
                let rest = self.lower_do_iter(stmts, tail);
                ir::Expr::Let(self.pat(pattern), Box::new(self.expr(value)), Box::new(rest))
            }
            Some(DoStmt::Seq(expr)) => {
                let rest = self.lower_do_iter(stmts, tail);
                ir::Expr::Let(ir::Pat::Wild, Box::new(self.expr(expr)), Box::new(rest))
            }
            Some(DoStmt::Bind { pattern, value }) => {
                let val_id = value.id;
                let val_ty = self.node_types.get(&val_id).cloned();
                let val_ir = self.expr(value);
                let rest = self.lower_do_iter(stmts, tail);
                let lam = ir::Expr::Lam(self.pat(pattern), Box::new(rest));

                // Resolve `Monad.and_then` by extracting the monad constructor
                // from `val_ty` (`m a` → outer head = `m`).
                let and_then = val_ty.as_ref().and_then(|ty| {
                    let (head, _) = ty.flatten_app();
                    let type_name = match head {
                        Ty::Con(name) => Some(name.clone()),
                        _ => None,
                    }?;
                    if let Some(entry) = self.global.impls.get(&("Monad".to_string(), type_name)) {
                        let dict_expr = make_dict_ref(&entry.dict_ident, &entry.module_var);
                        Some(ir::Expr::Field(Box::new(dict_expr), "and_then".to_string()))
                    } else {
                        // Fallback: try the full resolve_trait_call_with_ty path
                        // (handles parameterized / dict-param impls).
                        self.resolve_trait_call_with_ty("Monad", "and_then", ty)
                    }
                });

                match and_then {
                    Some(f) => ir::Expr::App(
                        Box::new(ir::Expr::App(Box::new(f), Box::new(val_ir))),
                        Box::new(lam),
                    ),
                    None => {
                        // Fallback: emit `val_ir` and `lam` side-by-side (should not happen
                        // in well-typed code; the type checker would have caught this).
                        ir::Expr::Let(ir::Pat::Wild, Box::new(val_ir), Box::new(lam))
                    }
                }
            }
        }
    }



    // ── Patterns ─────────────────────────────────────────────────────────

    fn pat(&self, p: Pattern) -> ir::Pat {
        match p {
            Pattern::Wildcard => ir::Pat::Wild,
            Pattern::Ident(name, _, _) => ir::Pat::Var(name),
            Pattern::Literal(Literal::Number(n)) => ir::Pat::Lit(ir::Lit::Num(n)),
            Pattern::Literal(Literal::Text(s)) => ir::Pat::Lit(ir::Lit::Str(s)),
            Pattern::Literal(Literal::Bool(b)) => ir::Pat::Lit(ir::Lit::Bool(b)),
            Pattern::Variant { name, payload } => {
                ir::Pat::Tag(name, payload.map(|p| Box::new(self.pat(*p))))
            }
            Pattern::Record(rp) => ir::Pat::Record {
                fields: rp
                    .fields
                    .into_iter()
                    .map(|f| (f.name, f.pattern.map(|p| self.pat(p))))
                    .collect(),
                rest: rp.rest.map(|r| r.map(|(name, _, _)| name)),
            },
            Pattern::List(lp) => ir::Pat::List {
                elems: lp.elements.into_iter().map(|p| self.pat(p)).collect(),
                rest: lp.rest.map(|r| r.map(|(name, _, _)| name)),
            },
        }
    }

    // ── Trait resolution ─────────────────────────────────────────────────

    fn resolve_trait_call(
        &self,
        node_id: NodeId,
        trait_name: &str,
        method_name: &str,
    ) -> Option<ir::Expr> {
        let call_ty = self.node_types.get(&node_id)?;
        let type_name = self.extract_type_param(trait_name, method_name, call_ty)?;

        // Exact match in concrete impls.
        if let Some(entry) = self.global.impls.get(&(trait_name.to_string(), type_name.clone())) {
            let dict_expr = make_dict_ref(&entry.dict_ident, &entry.module_var);
            return Some(ir::Expr::Field(Box::new(dict_expr), method_name.to_string()));
        }

        // Fallback: parameterized impls.
        let trait_def = self.global.traits.get(trait_name)?;
        let method_decl = trait_def.methods.iter().find(|m| m.name == method_name)?;
        let type_at_param = find_ty_at_param(&method_decl.ty, &trait_def.type_param, call_ty)?;

        for pi in &self.global.param_impls {
            if pi.trait_name != trait_name {
                continue;
            }
            let bindings = match match_ast_type_against_ty(&pi.target_type, &type_at_param) {
                Some(b) => b,
                None => continue,
            };

            let mut dict_expr = make_dict_ref(&pi.dict_ident, &pi.module_var);

            for (c_trait, c_var) in &pi.constraints {
                let bound_ty = match bindings.get(c_var) {
                    Some(t) => t,
                    None => continue,
                };
                let dict_arg = self.resolve_dict_arg(c_trait, bound_ty)?;
                dict_expr = ir::Expr::App(Box::new(dict_expr), Box::new(dict_arg));
            }

            return Some(ir::Expr::Field(Box::new(dict_expr), method_name.to_string()));
        }

        None
    }

    /// Like `resolve_trait_call` but uses an explicitly-provided type
    /// (the operator's instantiated type) instead of looking up in `node_types`.
    fn resolve_trait_call_with_ty(
        &self,
        trait_name: &str,
        method_name: &str,
        call_ty: &Ty,
    ) -> Option<ir::Expr> {
        let type_name = self.extract_type_param(trait_name, method_name, call_ty)?;

        // Exact match in concrete impls.
        if let Some(entry) = self.global.impls.get(&(trait_name.to_string(), type_name.clone())) {
            let dict_expr = make_dict_ref(&entry.dict_ident, &entry.module_var);
            return Some(ir::Expr::Field(Box::new(dict_expr), method_name.to_string()));
        }

        // Fallback: parameterized impls.
        let trait_def = self.global.traits.get(trait_name)?;
        let method_decl = trait_def.methods.iter().find(|m| m.name == method_name)?;
        let type_at_param = find_ty_at_param(&method_decl.ty, &trait_def.type_param, call_ty)?;

        for pi in &self.global.param_impls {
            if pi.trait_name != trait_name {
                continue;
            }
            let bindings = match match_ast_type_against_ty(&pi.target_type, &type_at_param) {
                Some(b) => b,
                None => continue,
            };

            let mut dict_expr = make_dict_ref(&pi.dict_ident, &pi.module_var);

            for (c_trait, c_var) in &pi.constraints {
                let bound_ty = match bindings.get(c_var) {
                    Some(t) => t,
                    None => continue,
                };
                let dict_arg = self.resolve_dict_arg(c_trait, bound_ty)?;
                dict_expr = ir::Expr::App(Box::new(dict_expr), Box::new(dict_arg));
            }

            return Some(ir::Expr::Field(Box::new(dict_expr), method_name.to_string()));
        }

        // Dict param resolution (polymorphic context).
        let trait_def2 = self.global.traits.get(trait_name)?;
        let method_decl2 = trait_def2.methods.iter().find(|m| m.name == method_name)?;
        let ty_at_param = find_ty_at_param(&method_decl2.ty, &trait_def2.type_param, call_ty)?;
        if let Ty::Var(v) = ty_at_param {
            if let Some((dict_name, _)) = self.dict_params.get(&v) {
                return Some(ir::Expr::Field(
                    Box::new(ir::Expr::Var(dict_name.clone())),
                    method_name.to_string(),
                ));
            }
        }

        None
    }

    fn resolve_trait_call_via_dict(
        &self,
        node_id: NodeId,
        trait_name: &str,
        method_name: &str,
    ) -> Option<ir::Expr> {
        let call_ty = self.node_types.get(&node_id)?;
        let trait_def = self.global.traits.get(trait_name)?;
        let method_decl = trait_def.methods.iter().find(|m| m.name == method_name)?;
        let ty_at_param = find_ty_at_param(&method_decl.ty, &trait_def.type_param, call_ty)?;
        if let Ty::Var(v) = ty_at_param {
            if let Some((dict_name, _)) = self.dict_params.get(&v) {
                return Some(ir::Expr::Field(
                    Box::new(ir::Expr::Var(dict_name.clone())),
                    method_name.to_string(),
                ));
            }
        }
        None
    }

    /// At a call site for a constrained ident, insert dict arguments.
    fn insert_dict_args(
        &self,
        node_id: NodeId,
        name: &str,
    ) -> Option<ir::Expr> {
        let scheme = self.type_env.lookup(name)?;
        if scheme.constraints.is_empty() {
            return None;
        }
        let call_ty = self.node_types.get(&node_id)?;

        let mut var_map: HashMap<TyVar, Ty> = HashMap::new();
        match_types(&scheme.ty, call_ty, &mut var_map);

        let mut result = ir::Expr::Var(name.to_string());
        let mut any_dict = false;

        for (trait_name, var) in &scheme.constraints {
            let dict_arg = if let Some(concrete_ty) = var_map.get(var) {
                if let Ty::Var(v) = concrete_ty {
                    if let Some((dict_name, _)) = self.dict_params.get(v) {
                        ir::Expr::Var(dict_name.clone())
                    } else {
                        // Type variable not in dict_params — the constraint is
                        // still polymorphic at this call site; skip gracefully.
                        continue;
                    }
                } else {
                    if let Some(arg) = self.resolve_dict_arg(trait_name, concrete_ty) {
                        arg
                    } else {
                        // No impl found — emit a runtime error rather than
                        // silently producing a partially-applied function.
                        ir::Expr::App(
                            Box::new(ir::Expr::Var("__error".into())),
                            Box::new(ir::Expr::Str(format!(
                                "missing impl: {} for {}",
                                trait_name,
                                ty_canonical_name(concrete_ty)
                                    .unwrap_or_else(|| "unknown".into())
                            ))),
                        )
                    }
                }
            } else {
                // Constraint variable not in var_map — type is unknown at this
                // site (still polymorphic); skip gracefully.
                continue;
            };

            any_dict = true;
            result = ir::Expr::App(Box::new(result), Box::new(dict_arg));
        }

        if any_dict { Some(result) } else { None }
    }

    fn resolve_dict_arg(&self, trait_name: &str, concrete_ty: &Ty) -> Option<ir::Expr> {
        let type_name = ty_canonical_name(concrete_ty)?;
        if let Some(entry) = self.global.impls.get(&(trait_name.to_string(), type_name)) {
            return Some(make_dict_ref(&entry.dict_ident, &entry.module_var));
        }
        for pi in &self.global.param_impls {
            if pi.trait_name != trait_name {
                continue;
            }
            let bindings = match match_ast_type_against_ty(&pi.target_type, concrete_ty) {
                Some(b) => b,
                None => continue,
            };
            let mut dict_expr = make_dict_ref(&pi.dict_ident, &pi.module_var);
            for (c_trait, c_var) in &pi.constraints {
                let bound_ty = bindings.get(c_var)?;
                let dict_arg = self.resolve_dict_arg(c_trait, bound_ty)?;
                dict_expr = ir::Expr::App(Box::new(dict_expr), Box::new(dict_arg));
            }
            return Some(dict_expr);
        }
        None
    }

    fn extract_type_param(
        &self,
        trait_name: &str,
        method_name: &str,
        concrete: &Ty,
    ) -> Option<String> {
        let trait_def = self.global.traits.get(trait_name)?;
        let method_decl = trait_def.methods.iter().find(|m| m.name == method_name)?;
        find_type_param_in_ast_type(&method_decl.ty, &trait_def.type_param, concrete)
    }
}

// ── Free functions ───────────────────────────────────────────────────────────

fn make_dict_ref(dict_ident: &str, module_var: &Option<String>) -> ir::Expr {
    match module_var {
        None => ir::Expr::Var(dict_ident.to_string()),
        Some(mod_var) => ir::Expr::Field(
            Box::new(ir::Expr::Var(mod_var.clone())),
            dict_ident.to_string(),
        ),
    }
}

/// Canonical dict binding name: `Show` + `Num` → `__show_Num`.
pub fn dict_name(trait_name: &str, type_name: &str) -> String {
    let sanitized: String = type_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("__{}_{}", trait_name.to_ascii_lowercase(), sanitized)
}

// ── Type helpers ─────────────────────────────────────────────────────────────

/// Flatten a left-associative AST App tree into (base, [args]).
/// `App(App(Constructor("Result"), Var("a")), Var("b"))` → `(Constructor("Result"), [Var("a"), Var("b")])`
fn flatten_ast_app_lower(ty: &Type) -> (&Type, Vec<&Type>) {
    let mut cur = ty;
    let mut args = Vec::new();
    while let Type::App { callee, arg } = cur {
        args.push(arg.as_ref());
        cur = callee.as_ref();
    }
    args.reverse();
    (cur, args)
}

fn find_type_param_in_ast_type(ast_ty: &Type, param_name: &str, concrete: &Ty) -> Option<String> {
    match ast_ty {
        Type::Var(v) if v == param_name => ty_canonical_name(concrete),
        Type::Func { param, ret } => {
            if let Ty::Func(cp, cr) = concrete {
                find_type_param_in_ast_type(param, param_name, cp)
                    .or_else(|| find_type_param_in_ast_type(ret, param_name, cr))
            } else {
                None
            }
        }
        Type::Constructor(_) => None, // Concrete constructors can't be the trait param.
        Type::App { .. } => {
            let (ast_base, ast_args) = flatten_ast_app_lower(ast_ty);
            let (ty_head, ty_args) = concrete.flatten_app();
            // Check the base (e.g. `f` matching `Con("List")` for HKTs)
            find_type_param_in_ast_type(ast_base, param_name, ty_head)
                .or_else(|| {
                    ast_args.iter()
                        .zip(ty_args.iter())
                        .find_map(|(a, c)| find_type_param_in_ast_type(a, param_name, c))
                })
        }
        Type::Record(rt) => {
            if let Ty::Record(row) = concrete {
                rt.fields.iter().find_map(|f| {
                    row.fields
                        .iter()
                        .find(|(name, _)| name == &f.name)
                        .and_then(|(_, ty)| find_type_param_in_ast_type(&f.ty, param_name, ty))
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn find_ty_at_param(ast_ty: &Type, param_name: &str, concrete: &Ty) -> Option<Ty> {
    match ast_ty {
        Type::Var(v) if v == param_name => Some(concrete.clone()),
        Type::Func { param, ret } => {
            if let Ty::Func(cp, cr) = concrete {
                find_ty_at_param(param, param_name, cp)
                    .or_else(|| find_ty_at_param(ret, param_name, cr))
            } else {
                None
            }
        }
        Type::Constructor(_) => None, // Concrete constructors can't be the trait param.
        Type::App { .. } => {
            let (ast_base, ast_args) = flatten_ast_app_lower(ast_ty);
            let (ty_head, ty_args) = concrete.flatten_app();
            // Check the base (e.g. `f` matching `Con("List")` for HKTs)
            find_ty_at_param(ast_base, param_name, ty_head)
                .or_else(|| {
                    ast_args.iter()
                        .zip(ty_args.iter())
                        .find_map(|(a, c)| find_ty_at_param(a, param_name, c))
                })
        }
        Type::Record(rt) => {
            if let Ty::Record(row) = concrete {
                rt.fields.iter().find_map(|f| {
                    row.fields
                        .iter()
                        .find(|(name, _)| name == &f.name)
                        .and_then(|(_, ty)| find_ty_at_param(&f.ty, param_name, ty))
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn match_types(scheme_ty: &Ty, concrete_ty: &Ty, out: &mut HashMap<TyVar, Ty>) {
    match (scheme_ty, concrete_ty) {
        (Ty::Var(v), _) => {
            out.insert(*v, concrete_ty.clone());
        }
        (Ty::Func(sp, sr), Ty::Func(cp, cr)) => {
            match_types(sp, cp, out);
            match_types(sr, cr, out);
        }
        (Ty::App(sc, sa), Ty::App(cc, ca)) => {
            match_types(sc, cc, out);
            match_types(sa, ca, out);
        }
        (Ty::Con(sn), Ty::Con(cn)) if sn == cn => {}
        (Ty::Record(sr), Ty::Record(cr)) => {
            for (sname, sty) in &sr.fields {
                if let Some((_, cty)) = cr.fields.iter().find(|(n, _)| n == sname) {
                    match_types(sty, cty, out);
                }
            }
        }
        _ => {}
    }
}

fn ty_canonical_name(ty: &Ty) -> Option<String> {
    match ty {
        Ty::Num => Some("Num".to_string()),
        Ty::Text => Some("Text".to_string()),
        Ty::Bool => Some("Bool".to_string()),
        Ty::Con(name) => Some(name.clone()),
        Ty::App(..) => {
            let (head, args) = ty.flatten_app();
            let head_name = ty_canonical_name(head)?;
            if args.is_empty() {
                return Some(head_name);
            }
            let arg_strs: Option<Vec<String>> = args.iter().map(|a| ty_canonical_name(a)).collect();
            Some(format!("{} {}", head_name, arg_strs?.join(" ")))
        }
        Ty::Record(row) => {
            if !matches!(row.tail, crate::types::RowTail::Closed) {
                return None;
            }
            let mut sorted = row.fields.clone();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let field_strs: Option<Vec<String>> = sorted
                .iter()
                .map(|(name, ty)| ty_canonical_name(ty).map(|t| format!("{}: {}", name, t)))
                .collect();
            Some(format!("{{ {} }}", field_strs?.join(", ")))
        }
        _ => None,
    }
}

fn match_ast_type_against_ty(pattern: &Type, concrete: &Ty) -> Option<HashMap<String, Ty>> {
    let mut bindings = HashMap::new();
    if match_ast_inner(pattern, concrete, &mut bindings) {
        Some(bindings)
    } else {
        None
    }
}

fn match_ast_inner(
    pattern: &Type,
    concrete: &Ty,
    bindings: &mut HashMap<String, Ty>,
) -> bool {
    match (pattern, concrete) {
        (Type::Var(name), _) => {
            if let Some(existing) = bindings.get(name) {
                existing == concrete
            } else {
                bindings.insert(name.clone(), concrete.clone());
                true
            }
        }
        (Type::Constructor(name), _) => match (name.as_str(), concrete) {
            ("Num", Ty::Num) => true,
            ("Text", Ty::Text) => true,
            ("Bool", Ty::Bool) => true,
            (n, Ty::Con(cn)) => n == cn,
            _ => false,
        },
        (Type::App { .. }, _) => {
            let (ast_base, ast_args) = flatten_ast_app_lower(pattern);
            let (ty_head, ty_args) = concrete.flatten_app();
            if ast_args.len() != ty_args.len() {
                return false;
            }
            if !match_ast_inner(ast_base, ty_head, bindings) {
                return false;
            }
            ast_args.iter()
                .zip(ty_args.iter())
                .all(|(p, c)| match_ast_inner(p, c, bindings))
        }
        (Type::Func { param, ret }, Ty::Func(cp, cr)) => {
            match_ast_inner(param, cp, bindings) && match_ast_inner(ret, cr, bindings)
        }
        (Type::Record(rt), Ty::Record(row)) => {
            if rt.fields.len() != row.fields.len() {
                return false;
            }
            if !rt.open && !matches!(row.tail, crate::types::RowTail::Closed) {
                return false;
            }
            for ast_f in &rt.fields {
                if let Some((_, ty)) = row.fields.iter().find(|(n, _)| n == &ast_f.name) {
                    if !match_ast_inner(&ast_f.ty, ty, bindings) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}
