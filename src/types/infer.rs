use std::collections::{HashMap, HashSet};
use std::path::Path;
use crate::ast::{
    self, Binding, BinOp, Expr, ExprKind, Literal, ListPattern, MatchArm, Pattern,
    Program, RecordField, RecordPattern, TopItem, UnOp, UseBinding, UseDecl,
};
use crate::error::Span;
use crate::types::{
    free_row_vars, free_type_vars, unify, Row, RowTail, Scheme, Subst, Ty, TyVar,
    TypeError, TypeErrorAt,
};

// ── Type environment ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct TypeEnv(HashMap<String, Scheme>);

impl TypeEnv {
    pub fn new() -> Self { Self::default() }

    pub fn insert(&mut self, name: String, scheme: Scheme) {
        self.0.insert(name, scheme);
    }

    pub fn lookup(&self, name: &str) -> Option<&Scheme> {
        self.0.get(name)
    }

    pub fn extend_one(&self, name: String, ty: Ty) -> TypeEnv {
        let mut env = self.clone();
        env.insert(name, Scheme::mono(ty));
        env
    }

    pub fn extend_many(&self, bindings: Vec<(String, Ty)>) -> TypeEnv {
        let mut env = self.clone();
        for (name, ty) in bindings {
            env.insert(name, Scheme::mono(ty));
        }
        env
    }

    /// Free type vars and row vars across all schemes after applying `s`.
    fn free_vars(&self, s: &Subst) -> (HashSet<TyVar>, HashSet<TyVar>) {
        let mut tvs = HashSet::new();
        let mut rvs = HashSet::new();
        for scheme in self.0.values() {
            let scheme = s.apply_scheme(scheme);
            let quant_t: HashSet<TyVar> = scheme.vars.iter().copied().collect();
            let quant_r: HashSet<TyVar> = scheme.row_vars.iter().copied().collect();
            for v in free_type_vars(&scheme.ty).difference(&quant_t) { tvs.insert(*v); }
            for v in free_row_vars(&scheme.ty).difference(&quant_r) { rvs.insert(*v); }
        }
        (tvs, rvs)
    }
}

// ── Variant environment ───────────────────────────────────────────────────────

/// Metadata for a variant constructor.
#[derive(Debug, Clone)]
pub struct VariantInfo {
    /// The parent type name (e.g. `"Shape"`, `"Maybe"`).
    pub type_name: String,
    /// Named type parameters of the parent type (e.g. `["a"]` for `Maybe a`).
    pub type_params: Vec<String>,
    /// Payload fields in the AST representation (None for unit variants).
    /// Using AST types here so we can lower them fresh per instantiation.
    pub payload_fields: Option<Vec<(String, ast::Type)>>,
}

#[derive(Debug, Clone, Default)]
pub struct VariantEnv(HashMap<String, VariantInfo>);

impl VariantEnv {
    pub fn lookup(&self, name: &str) -> Option<&VariantInfo> { self.0.get(name) }
    pub fn insert(&mut self, name: String, info: VariantInfo) { self.0.insert(name, info); }
    pub fn merge(&mut self, other: VariantEnv) {
        for (k, v) in other.0 { self.0.insert(k, v); }
    }
}

// ── Build variant environment from type definitions ───────────────────────────

pub fn build_variant_env(items: &[TopItem]) -> VariantEnv {
    let mut env = VariantEnv::default();
    for item in items {
        if let TopItem::TypeDef(td) = item {
            for variant in &td.variants {
                let payload = variant.payload.as_ref().map(|rt| {
                    rt.fields.iter()
                        .map(|f| (f.name.clone(), f.ty.clone()))
                        .collect()
                });
                env.insert(variant.name.clone(), VariantInfo {
                    type_name: td.name.clone(),
                    type_params: td.params.clone(),
                    payload_fields: payload,
                });
            }
        }
    }
    env
}

// ── Built-in environment ──────────────────────────────────────────────────────

/// Initial (TypeEnv, VariantEnv) populated with Lume's standard library.
pub fn builtin_env() -> (TypeEnv, VariantEnv) {
    let mut env = TypeEnv::new();
    let mut var_env = VariantEnv::default();

    // Helper to create a scheme with N quantified type vars and 0 row vars.
    let mk_scheme = |vars: Vec<TyVar>, ty: Ty| Scheme { vars, row_vars: vec![], ty };

    // Basic functions
    env.insert("show".into(), mk_scheme(vec![0], Ty::Func(Box::new(Ty::Var(0)), Box::new(Ty::Text))));
    env.insert("not".into(), Scheme::mono(Ty::Func(Box::new(Ty::Bool), Box::new(Ty::Bool))));
    env.insert("abs".into(), Scheme::mono(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))));
    env.insert("round".into(), Scheme::mono(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))));
    env.insert("floor".into(), Scheme::mono(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))));
    env.insert("ceil".into(), Scheme::mono(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))));

    // Num -> Num -> Num
    let num2 = Ty::Func(Box::new(Ty::Num), Box::new(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))));
    env.insert("max".into(), Scheme::mono(num2.clone()));
    env.insert("min".into(), Scheme::mono(num2));

    // List functions — use vars 0, 1
    let list_a = Ty::List(Box::new(Ty::Var(0)));
    let list_b = Ty::List(Box::new(Ty::Var(1)));

    // map : (a -> b) -> List a -> List b
    env.insert("map".into(), mk_scheme(vec![0, 1], Ty::Func(
        Box::new(Ty::Func(Box::new(Ty::Var(0)), Box::new(Ty::Var(1)))),
        Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_b.clone()))),
    )));

    // filter : (a -> Bool) -> List a -> List a
    env.insert("filter".into(), mk_scheme(vec![0], Ty::Func(
        Box::new(Ty::Func(Box::new(Ty::Var(0)), Box::new(Ty::Bool))),
        Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone()))),
    )));

    // fold : b -> (b -> a -> b) -> List a -> b
    env.insert("fold".into(), mk_scheme(vec![0, 1], Ty::Func(
        Box::new(Ty::Var(1)),
        Box::new(Ty::Func(
            Box::new(Ty::Func(Box::new(Ty::Var(1)), Box::new(Ty::Func(Box::new(Ty::Var(0)), Box::new(Ty::Var(1)))))),
            Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(Ty::Var(1)))),
        )),
    )));

    // length : List a -> Num
    env.insert("length".into(), mk_scheme(vec![0], Ty::Func(Box::new(list_a.clone()), Box::new(Ty::Num))));
    // reverse, sort : List a -> List a
    env.insert("reverse".into(), mk_scheme(vec![0], Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone()))));
    env.insert("sort".into(), Scheme::mono(Ty::Func(
        Box::new(Ty::List(Box::new(Ty::Num))), Box::new(Ty::List(Box::new(Ty::Num))))));
    // take, drop : Num -> List a -> List a
    env.insert("take".into(), mk_scheme(vec![0], Ty::Func(
        Box::new(Ty::Num), Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone()))))));
    env.insert("drop".into(), mk_scheme(vec![0], Ty::Func(
        Box::new(Ty::Num), Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone()))))));
    // any, all : (a -> Bool) -> List a -> Bool
    let pred_a = Ty::Func(Box::new(Ty::Var(0)), Box::new(Ty::Bool));
    env.insert("any".into(), mk_scheme(vec![0], Ty::Func(
        Box::new(pred_a.clone()), Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(Ty::Bool))))));
    env.insert("all".into(), mk_scheme(vec![0], Ty::Func(
        Box::new(pred_a.clone()), Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(Ty::Bool))))));
    // average, sum : List Num -> Num
    let list_num = Ty::List(Box::new(Ty::Num));
    env.insert("average".into(), Scheme::mono(Ty::Func(Box::new(list_num.clone()), Box::new(Ty::Num))));
    env.insert("sum".into(), Scheme::mono(Ty::Func(Box::new(list_num.clone()), Box::new(Ty::Num))));
    // sortBy : (a -> Num) -> List a -> List a
    env.insert("sortBy".into(), mk_scheme(vec![0], Ty::Func(
        Box::new(Ty::Func(Box::new(Ty::Var(0)), Box::new(Ty::Num))),
        Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone()))),
    )));

    // print : Text -> {}
    env.insert("print".into(), Scheme::mono(Ty::Func(
        Box::new(Ty::Text),
        Box::new(Ty::Record(Row { fields: vec![], tail: RowTail::Closed })),
    )));

    // Text functions
    env.insert("trim".into(), Scheme::mono(Ty::Func(Box::new(Ty::Text), Box::new(Ty::Text))));
    env.insert("toUpper".into(), Scheme::mono(Ty::Func(Box::new(Ty::Text), Box::new(Ty::Text))));
    env.insert("toLower".into(), Scheme::mono(Ty::Func(Box::new(Ty::Text), Box::new(Ty::Text))));
    // split : Text -> Text -> List Text
    env.insert("split".into(), Scheme::mono(Ty::Func(
        Box::new(Ty::Text),
        Box::new(Ty::Func(Box::new(Ty::Text), Box::new(Ty::List(Box::new(Ty::Text))))))));
    // join : Text -> List Text -> Text
    env.insert("join".into(), Scheme::mono(Ty::Func(
        Box::new(Ty::Text),
        Box::new(Ty::Func(Box::new(Ty::List(Box::new(Ty::Text))), Box::new(Ty::Text))))));
    // contains, startsWith, endsWith : Text -> Text -> Bool
    let t2bool = Ty::Func(Box::new(Ty::Text), Box::new(Ty::Func(Box::new(Ty::Text), Box::new(Ty::Bool))));
    env.insert("contains".into(), Scheme::mono(t2bool.clone()));
    env.insert("startsWith".into(), Scheme::mono(t2bool.clone()));
    env.insert("endsWith".into(), Scheme::mono(t2bool));

    // Result helpers: unwrap : Result a e -> a
    env.insert("unwrap".into(), mk_scheme(vec![0, 1],
        Ty::Func(Box::new(Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)])), Box::new(Ty::Var(0)))));
    // withDefault : a -> Maybe a -> a
    env.insert("withDefault".into(), mk_scheme(vec![0], Ty::Func(
        Box::new(Ty::Var(0)),
        Box::new(Ty::Func(Box::new(Ty::Con("Maybe".into(), vec![Ty::Var(0)])), Box::new(Ty::Var(0)))))));
    // mapErr : (e -> f) -> Result a e -> Result a f
    env.insert("mapErr".into(), mk_scheme(vec![0, 1, 2], Ty::Func(
        Box::new(Ty::Func(Box::new(Ty::Var(1)), Box::new(Ty::Var(2)))),
        Box::new(Ty::Func(
            Box::new(Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)])),
            Box::new(Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(2)])),
        )),
    )));

    // Maybe variants: Some { value: a }, None
    var_env.insert("Some".into(), VariantInfo {
        type_name: "Maybe".into(),
        type_params: vec!["a".into()],
        payload_fields: Some(vec![
            ("value".into(), ast::Type::Var("a".into())),
        ]),
    });
    var_env.insert("None".into(), VariantInfo {
        type_name: "Maybe".into(),
        type_params: vec!["a".into()],
        payload_fields: None,
    });

    // Result variants: Ok { value: a }, Err { reason: b }
    var_env.insert("Ok".into(), VariantInfo {
        type_name: "Result".into(),
        type_params: vec!["a".into(), "b".into()],
        payload_fields: Some(vec![
            ("value".into(), ast::Type::Var("a".into())),
        ]),
    });
    var_env.insert("Err".into(), VariantInfo {
        type_name: "Result".into(),
        type_params: vec!["a".into(), "b".into()],
        payload_fields: Some(vec![
            ("reason".into(), ast::Type::Var("b".into())),
        ]),
    });

    (env, var_env)
}

/// Return type of `instantiate_variant`: (result_type, optional payload fields).
type VariantInstance = (Ty, Option<Vec<(String, Ty)>>);

// ── The typechecker ───────────────────────────────────────────────────────────

pub struct Checker {
    pub subst: Subst,
    pub variant_env: VariantEnv,
}

impl Checker {
    pub fn new(variant_env: VariantEnv) -> Self {
        Checker { subst: Subst::new(), variant_env }
    }

    fn fresh_var(&mut self) -> TyVar { self.subst.fresh_var() }
    fn fresh_ty(&mut self) -> Ty { self.subst.fresh_ty() }

    // ── Instantiation & generalisation ──────────────────────────────────────

    /// Replace each quantified var with a fresh variable.
    fn instantiate(&mut self, scheme: &Scheme) -> Ty {
        if scheme.vars.is_empty() && scheme.row_vars.is_empty() {
            return scheme.ty.clone();
        }
        let mut local_tys: HashMap<TyVar, Ty> = HashMap::new();
        let mut local_rows: HashMap<TyVar, Row> = HashMap::new();
        for &v in &scheme.vars {
            local_tys.insert(v, self.fresh_ty());
        }
        for &v in &scheme.row_vars {
            local_rows.insert(v, Row { fields: vec![], tail: RowTail::Open(self.fresh_var()) });
        }
        let tmp = Subst { counter: self.subst.counter, tys: local_tys, rows: local_rows };
        tmp.apply(&scheme.ty)
    }

    /// Quantify over type and row variables that are free in `ty` but not in `env`.
    fn generalise(&self, env: &TypeEnv, ty: &Ty) -> Scheme {
        let ty = self.subst.apply(ty);
        let (env_tvs, env_rvs) = env.free_vars(&self.subst);
        let ty_tvs = free_type_vars(&ty);
        let ty_rvs = free_row_vars(&ty);
        Scheme {
            vars: ty_tvs.difference(&env_tvs).copied().collect(),
            row_vars: ty_rvs.difference(&env_rvs).copied().collect(),
            ty,
        }
    }

    fn unify(&mut self, t1: Ty, t2: Ty) -> Result<(), TypeError> {
        unify(&mut self.subst, t1, t2)
    }

    /// Convenience: unify and wrap any error with the given span.
    fn unify_at(&mut self, t1: Ty, t2: Ty, span: &Span) -> Result<(), TypeErrorAt> {
        self.unify(t1, t2).map_err(|e| TypeErrorAt::new(e, span.clone()))
    }

    // ── AST type lowering ────────────────────────────────────────────────────

    /// Convert an AST `Type` to an internal `Ty`.
    /// `param_vars` maps type parameter *names* (e.g. `"a"`) to their fresh `TyVar`.
    fn lower_ty(
        &mut self,
        ty: &ast::Type,
        param_vars: &mut HashMap<String, TyVar>,
    ) -> Result<Ty, TypeError> {
        use ast::Type;
        match ty {
            Type::Named { name, args } => match name.as_str() {
                "Num"  => Ok(Ty::Num),
                "Text" => Ok(Ty::Text),
                "Bool" => Ok(Ty::Bool),
                "List" if args.len() == 1 => {
                    let inner = self.lower_ty(&args[0], param_vars)?;
                    Ok(Ty::List(Box::new(inner)))
                }
                _ => {
                    let conv: Result<Vec<_>, _> = args.iter()
                        .map(|a| self.lower_ty(a, param_vars))
                        .collect();
                    Ok(Ty::Con(name.clone(), conv?))
                }
            },
            Type::Var(name) => {
                let v = param_vars.entry(name.clone())
                    .or_insert_with(|| self.subst.fresh_var());
                Ok(Ty::Var(*v))
            }
            Type::Record(rt) => {
                let mut fields: Vec<(String, Ty)> = rt.fields.iter()
                    .map(|f| self.lower_ty(&f.ty, param_vars).map(|t| (f.name.clone(), t)))
                    .collect::<Result<_, _>>()?;
                fields.sort_by(|a, b| a.0.cmp(&b.0));
                let tail = if rt.open { RowTail::Open(self.fresh_var()) } else { RowTail::Closed };
                Ok(Ty::Record(Row { fields, tail }))
            }
            Type::Func { param, ret } => {
                let tp = self.lower_ty(param, param_vars)?;
                let tr = self.lower_ty(ret, param_vars)?;
                Ok(Ty::Func(Box::new(tp), Box::new(tr)))
            }
        }
    }

    // ── Variant helpers ──────────────────────────────────────────────────────

    /// Instantiate a variant constructor, returning (result_type, payload_row_type).
    /// `payload_row_type` is None for unit variants.
    fn instantiate_variant(
        &mut self,
        name: &str,
    ) -> Result<VariantInstance, TypeError> {
        let info = self.variant_env.lookup(name)
            .ok_or_else(|| TypeError::UnboundVariant(name.to_string()))?
            .clone();  // clone to release the borrow before calling lower_ty

        // Map each type param name → fresh TyVar
        let mut param_vars: HashMap<String, TyVar> = HashMap::new();
        let fresh_args: Vec<Ty> = info.type_params.iter()
            .map(|p| {
                let v = self.fresh_var();
                param_vars.insert(p.clone(), v);
                Ty::Var(v)
            })
            .collect();

        let result_ty = Ty::Con(info.type_name.clone(), fresh_args);

        let payload_ty = if let Some(fields) = info.payload_fields {
            let mut converted: Vec<(String, Ty)> = fields.iter()
                .map(|(fname, fty)| {
                    self.lower_ty(fty, &mut param_vars).map(|t| (fname.clone(), t))
                })
                .collect::<Result<_, _>>()?;
            converted.sort_by(|a, b| a.0.cmp(&b.0));
            Some(converted)
        } else {
            None
        };

        Ok((result_ty, payload_ty))
    }

    // ── Inference (⇒ mode) ───────────────────────────────────────────────────

    pub fn infer(&mut self, env: &TypeEnv, expr: &Expr) -> Result<Ty, TypeErrorAt> {
        let span = &expr.span;
        match &expr.kind {
            ExprKind::Number(_) => Ok(Ty::Num),
            ExprKind::Text(_) => Ok(Ty::Text),
            ExprKind::Bool(_) => Ok(Ty::Bool),

            ExprKind::List(exprs) => {
                let elem = self.fresh_ty();
                for e in exprs {
                    let t = self.infer(env, e)?;
                    let t = self.subst.apply(&t);
                    let elem_c = self.subst.apply(&elem);
                    self.unify_at(t, elem_c, &e.span)?;
                }
                Ok(Ty::List(Box::new(self.subst.apply(&elem))))
            }

            ExprKind::Ident(name) => match env.lookup(name) {
                Some(scheme) => Ok(self.instantiate(scheme)),
                None => Err(TypeErrorAt::new(TypeError::UnboundVariable(name.clone()), span.clone())),
            },

            ExprKind::Variant { name, payload: None } => {
                let (result_ty, payload_ty) = self.instantiate_variant(name)
                    .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                if let Some(fields) = payload_ty {
                    // Has a payload but none was given; return as a constructor function.
                    let row = Row { fields, tail: RowTail::Closed };
                    let result_ty_c = result_ty.clone();
                    Ok(Ty::Func(Box::new(Ty::Record(row)), Box::new(result_ty_c)))
                } else {
                    Ok(result_ty)
                }
            }

            ExprKind::Variant { name, payload: Some(payload_expr) } => {
                let (result_ty, payload_fields) = self.instantiate_variant(name)
                    .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                let fields = payload_fields.ok_or_else(|| TypeErrorAt::new(
                    TypeError::UnboundVariant(format!("{} is a unit variant", name)),
                    span.clone(),
                ))?;
                let expected = Ty::Record(Row { fields, tail: RowTail::Closed });
                self.check(env, payload_expr, expected)?;
                Ok(result_ty)
            }

            ExprKind::Record { base: None, fields, .. } => {
                self.infer_record(env, fields, span)
            }

            ExprKind::Record { base: Some(base), fields, .. } => {
                self.infer_record_update(env, base, fields)
            }

            ExprKind::FieldAccess { record, field } => {
                self.infer_field_access(env, record, field)
            }

            ExprKind::Lambda { param, body } => {
                let param_ty = self.fresh_ty();
                let bindings = self.infer_pattern(param, param_ty.clone())
                    .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                let new_env = env.extend_many(bindings);
                let body_ty = self.infer(&new_env, body)?;
                let param_ty = self.subst.apply(&param_ty);
                let body_ty = self.subst.apply(&body_ty);
                Ok(Ty::Func(Box::new(param_ty), Box::new(body_ty)))
            }

            ExprKind::Apply { func, arg } => {
                let func_ty = self.infer(env, func)?;
                let arg_ty = self.infer(env, arg)?;
                let ret = self.fresh_ty();
                let func_ty = self.subst.apply(&func_ty);
                let arg_ty = self.subst.apply(&arg_ty);
                let ret_c = ret.clone();
                self.unify_at(func_ty, Ty::Func(Box::new(arg_ty), Box::new(ret)), &func.span)?;
                Ok(self.subst.apply(&ret_c))
            }

            ExprKind::If { cond, then_branch, else_branch } => {
                let cond_ty = self.infer(env, cond)?;
                let cond_ty = self.subst.apply(&cond_ty);
                self.unify_at(cond_ty, Ty::Bool, &cond.span)?;
                let then_ty = self.infer(env, then_branch)?;
                let else_ty = self.infer(env, else_branch)?;
                let then_ty = self.subst.apply(&then_ty);
                let else_ty = self.subst.apply(&else_ty);
                self.unify_at(then_ty.clone(), else_ty, &else_branch.span)?;
                Ok(self.subst.apply(&then_ty))
            }

            ExprKind::Binary { op, left, right } => {
                self.infer_binary(env, op, left, right)
            }

            ExprKind::Unary { op, operand } => match op {
                UnOp::Neg => {
                    let t = self.infer(env, operand)?;
                    let t = self.subst.apply(&t);
                    self.unify_at(t, Ty::Num, &operand.span)?;
                    Ok(Ty::Num)
                }
                UnOp::Not => {
                    let t = self.infer(env, operand)?;
                    let t = self.subst.apply(&t);
                    self.unify_at(t, Ty::Bool, &operand.span)?;
                    Ok(Ty::Bool)
                }
            },

            ExprKind::Match(arms) => {
                self.infer_match(env, arms, span)
            }
        }
    }

    fn infer_record(
        &mut self, env: &TypeEnv, fields: &[RecordField], span: &Span,
    ) -> Result<Ty, TypeErrorAt> {
        let mut row_fields: Vec<(String, Ty)> = Vec::new();
        for f in fields {
            let ty = if let Some(val) = &f.value {
                self.infer(env, val)?
            } else {
                match env.lookup(&f.name) {
                    Some(s) => self.instantiate(s),
                    None => return Err(TypeErrorAt::new(
                        TypeError::UnboundVariable(f.name.clone()), span.clone())),
                }
            };
            row_fields.push((f.name.clone(), self.subst.apply(&ty)));
        }
        row_fields.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Ty::Record(Row { fields: row_fields, tail: RowTail::Closed }))
    }

    fn infer_record_update(
        &mut self, env: &TypeEnv, base: &Expr, overrides: &[RecordField],
    ) -> Result<Ty, TypeErrorAt> {
        let base_ty = self.infer(env, base)?;
        let base_ty = self.subst.apply(&base_ty);

        let mut new_fields: Vec<(String, Ty)> = Vec::new();
        for f in overrides {
            let ty = if let Some(val) = &f.value {
                self.infer(env, val)?
            } else {
                match env.lookup(&f.name) {
                    Some(s) => self.instantiate(s),
                    None => return Err(TypeErrorAt::new(
                        TypeError::UnboundVariable(f.name.clone()), base.span.clone())),
                }
            };
            new_fields.push((f.name.clone(), self.subst.apply(&ty)));
        }
        new_fields.sort_by(|a, b| a.0.cmp(&b.0));

        // Unify base with an empty open row to capture all its fields into base_rv.
        let base_rv = self.fresh_var();
        let open_base = Ty::Record(Row { fields: vec![], tail: RowTail::Open(base_rv) });
        self.unify_at(base_ty, open_base, &base.span)?;

        Ok(Ty::Record(Row { fields: new_fields, tail: RowTail::Open(base_rv) }))
    }

    fn infer_field_access(
        &mut self, env: &TypeEnv, record_expr: &Expr, field: &str,
    ) -> Result<Ty, TypeErrorAt> {
        let span = &record_expr.span;
        let rec_ty = self.infer(env, record_expr)?;
        let rec_ty = self.subst.apply(&rec_ty);

        // Fast path: known record type.
        if let Ty::Record(row) = &rec_ty {
            let row = self.subst.apply_row(row);
            if let Some((_, t)) = row.fields.iter().find(|(k, _)| k == field) {
                return Ok(t.clone());
            }
            if let RowTail::Open(v) = row.tail {
                let field_ty = self.fresh_ty();
                let rest_var = self.fresh_var();
                self.subst.bind_row(v, Row {
                    fields: vec![(field.to_string(), field_ty.clone())],
                    tail: RowTail::Open(rest_var),
                });
                return Ok(self.subst.apply(&field_ty));
            }
            return Err(TypeErrorAt::new(
                TypeError::FieldNotFound { field: field.to_string(), record_ty: rec_ty },
                span.clone(),
            ));
        }

        // Generic case: constrain the record to have this field.
        let field_ty = self.fresh_ty();
        let row_tail = self.fresh_var();
        let expected = Ty::Record(Row {
            fields: vec![(field.to_string(), field_ty.clone())],
            tail: RowTail::Open(row_tail),
        });
        self.unify_at(rec_ty, expected, span)?;
        Ok(self.subst.apply(&field_ty))
    }

    fn infer_binary(
        &mut self, env: &TypeEnv, op: &BinOp, left: &Expr, right: &Expr,
    ) -> Result<Ty, TypeErrorAt> {
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                let tl = self.infer(env, left)?;
                let tl = self.subst.apply(&tl);
                self.unify_at(tl, Ty::Num, &left.span)?;
                let tr = self.infer(env, right)?;
                let tr = self.subst.apply(&tr);
                self.unify_at(tr, Ty::Num, &right.span)?;
                Ok(Ty::Num)
            }
            BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => {
                let tl = self.infer(env, left)?;
                let tl = self.subst.apply(&tl);
                self.unify_at(tl, Ty::Num, &left.span)?;
                let tr = self.infer(env, right)?;
                let tr = self.subst.apply(&tr);
                self.unify_at(tr, Ty::Num, &right.span)?;
                Ok(Ty::Bool)
            }
            BinOp::Eq | BinOp::NotEq => {
                let tl = self.infer(env, left)?;
                let tr = self.infer(env, right)?;
                let tl = self.subst.apply(&tl);
                let tr = self.subst.apply(&tr);
                self.unify_at(tl, tr, &right.span)?;
                Ok(Ty::Bool)
            }
            BinOp::And | BinOp::Or => {
                let tl = self.infer(env, left)?;
                let tl = self.subst.apply(&tl);
                self.unify_at(tl, Ty::Bool, &left.span)?;
                let tr = self.infer(env, right)?;
                let tr = self.subst.apply(&tr);
                self.unify_at(tr, Ty::Bool, &right.span)?;
                Ok(Ty::Bool)
            }
            BinOp::Concat => self.infer_concat(env, left, right),
            BinOp::Pipe => {
                // a |> f  ≡  f a
                let t_arg = self.infer(env, left)?;
                let t_func = self.infer(env, right)?;
                let t_arg = self.subst.apply(&t_arg);
                let t_func = self.subst.apply(&t_func);
                let ret = self.fresh_ty();
                let ret_c = ret.clone();
                self.unify_at(t_func, Ty::Func(Box::new(t_arg), Box::new(ret)), &right.span)?;
                Ok(self.subst.apply(&ret_c))
            }
            BinOp::ResultPipe => self.infer_result_pipe(env, left, right),
        }
    }

    fn infer_concat(
        &mut self, env: &TypeEnv, left: &Expr, right: &Expr,
    ) -> Result<Ty, TypeErrorAt> {
        let tl = self.infer(env, left)?;
        let tl = self.subst.apply(&tl);
        match &tl {
            Ty::Text => {
                self.check(env, right, Ty::Text)?;
                Ok(Ty::Text)
            }
            Ty::List(elem) => {
                let elem = *elem.clone();
                self.check(env, right, Ty::List(Box::new(elem.clone())))?;
                Ok(Ty::List(Box::new(self.subst.apply(&elem))))
            }
            Ty::Record(_) => {
                let tr = self.infer(env, right)?;
                let tr = self.subst.apply(&tr);
                if let Ty::Record(_) = &tr {
                    let rv = self.fresh_var();
                    Ok(Ty::Record(Row { fields: vec![], tail: RowTail::Open(rv) }))
                } else {
                    Err(TypeErrorAt::new(TypeError::Mismatch(tl, tr), right.span.clone()))
                }
            }
            Ty::Var(_) => {
                let tr = self.infer(env, right)?;
                let tr = self.subst.apply(&tr);
                let tl = self.subst.apply(&tl);
                self.unify_at(tl, tr.clone(), &right.span)?;
                Ok(self.subst.apply(&tr))
            }
            t => Err(TypeErrorAt::new(
                TypeError::ConcatNonConcatenable(t.clone()), left.span.clone())),
        }
    }

    fn infer_result_pipe(
        &mut self, env: &TypeEnv, left: &Expr, right: &Expr,
    ) -> Result<Ty, TypeErrorAt> {
        // left : Result a e ;  right : a -> Result b e ;  result : Result b e
        let tl = self.infer(env, left)?;
        let tl = self.subst.apply(&tl);
        let a = self.fresh_ty();
        let e = self.fresh_ty();
        let b = self.fresh_ty();
        let tl_c = tl.clone();
        self.unify(tl, Ty::Con("Result".into(), vec![a.clone(), e.clone()]))
            .map_err(|_| TypeErrorAt::new(TypeError::ResultPipeNonResult(tl_c), left.span.clone()))?;
        let tr = self.infer(env, right)?;
        let tr = self.subst.apply(&tr);
        let a = self.subst.apply(&a);
        let e = self.subst.apply(&e);
        let b_c = b.clone();
        self.unify_at(tr, Ty::Func(
            Box::new(a),
            Box::new(Ty::Con("Result".into(), vec![b, e.clone()]))), &right.span)?;
        let b = self.subst.apply(&b_c);
        let e = self.subst.apply(&e);
        Ok(Ty::Con("Result".into(), vec![b, e]))
    }

    fn infer_match(&mut self, env: &TypeEnv, arms: &[MatchArm], span: &Span) -> Result<Ty, TypeErrorAt> {
        let t_in = self.fresh_ty();
        let t_out = self.fresh_ty();

        for arm in arms {
            let t_in_c = self.subst.apply(&t_in);
            let bindings = self.infer_pattern(&arm.pattern, t_in_c)
                .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
            let arm_env = env.extend_many(bindings);

            if let Some(guard) = &arm.guard {
                let tg = self.infer(&arm_env, guard)?;
                let tg = self.subst.apply(&tg);
                let tg_c = tg.clone();
                self.unify(tg, Ty::Bool)
                    .map_err(|_| TypeErrorAt::new(TypeError::GuardNotBool(tg_c), guard.span.clone()))?;
            }

            let t_body = self.infer(&arm_env, &arm.body)?;
            let t_body = self.subst.apply(&t_body);
            let t_out_c = self.subst.apply(&t_out);
            self.unify_at(t_body, t_out_c, &arm.body.span)?;
        }

        let t_in = self.subst.apply(&t_in);
        let t_out = self.subst.apply(&t_out);
        Ok(Ty::Func(Box::new(t_in), Box::new(t_out)))
    }

    // ── Check mode (⇐) ───────────────────────────────────────────────────────

    /// Check `expr` against `expected`, using bidirectional rules where possible.
    pub fn check(&mut self, env: &TypeEnv, expr: &Expr, expected: Ty) -> Result<(), TypeErrorAt> {
        let span = &expr.span;
        let expected = self.subst.apply(&expected);
        match &expr.kind {
            // ── Lambda in check mode: propagate param type directly ──────────
            ExprKind::Lambda { param, body } => {
                if let Ty::Func(t_param, t_ret) = expected {
                    let bindings = self.infer_pattern(param, *t_param)
                        .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                    let new_env = env.extend_many(bindings);
                    self.check(&new_env, body, *t_ret)
                } else {
                    let inferred = self.infer(env, expr)?;
                    let inferred = self.subst.apply(&inferred);
                    self.unify_at(inferred, expected, span)
                }
            }
            // All other forms: infer + unify.
            _ => {
                let inferred = self.infer(env, expr)?;
                let inferred = self.subst.apply(&inferred);
                self.unify_at(inferred, expected, span)
            }
        }
    }

    // ── Pattern checking ─────────────────────────────────────────────────────

    /// Check `pat` against `expected`; returns (name, type) bindings.
    /// Pattern errors don't carry expression spans; callers convert as needed.
    pub fn infer_pattern(
        &mut self, pat: &Pattern, expected: Ty,
    ) -> Result<Vec<(String, Ty)>, TypeError> {
        let expected = self.subst.apply(&expected);
        match pat {
            Pattern::Wildcard => Ok(vec![]),

            Pattern::Literal(lit) => {
                let lit_ty = match lit {
                    Literal::Number(_) => Ty::Num,
                    Literal::Text(_)   => Ty::Text,
                    Literal::Bool(_)   => Ty::Bool,
                };
                self.unify(expected, lit_ty)?;
                Ok(vec![])
            }

            Pattern::Ident(name) => Ok(vec![(name.clone(), expected)]),

            Pattern::Variant { name, payload } => {
                self.check_variant_pattern(name, payload.as_deref(), expected)
            }

            Pattern::Record(rp) => self.check_record_pattern(rp, expected),

            Pattern::List(lp) => self.check_list_pattern(lp, expected),
        }
    }

    fn check_variant_pattern(
        &mut self,
        name: &str,
        payload: Option<&Pattern>,
        expected: Ty,
    ) -> Result<Vec<(String, Ty)>, TypeError> {
        let (result_ty, payload_fields) = self.instantiate_variant(name)?;
        self.unify(expected, result_ty)?;

        match (payload_fields, payload) {
            (None, None) | (None, Some(Pattern::Wildcard)) => Ok(vec![]),
            (Some(_), None) => Ok(vec![]),  // payload ignored
            (Some(fields), Some(p)) => {
                let payload_ty = Ty::Record(Row { fields, tail: RowTail::Closed });
                self.infer_pattern(p, payload_ty)
            }
            (None, Some(p)) => {
                // Unit variant but pattern given — wildcard fallback
                self.infer_pattern(p, Ty::Record(Row { fields: vec![], tail: RowTail::Closed }))
            }
        }
    }

    fn check_record_pattern(
        &mut self, rp: &RecordPattern, expected: Ty,
    ) -> Result<Vec<(String, Ty)>, TypeError> {
        let mut field_tys: Vec<(String, Ty)> = rp.fields.iter()
            .map(|fp| (fp.name.clone(), self.fresh_ty()))
            .collect();
        field_tys.sort_by(|a, b| a.0.cmp(&b.0));

        let tail = match &rp.rest {
            None => RowTail::Closed,
            Some(_) => RowTail::Open(self.fresh_var()),
        };
        let row_ty = Ty::Record(Row { fields: field_tys.clone(), tail: tail.clone() });
        self.unify(expected, row_ty)?;

        let mut bindings = Vec::new();

        for fp in &rp.fields {
            let field_ty = field_tys.iter().find(|(k, _)| k == &fp.name)
                .map(|(_, t)| self.subst.apply(t))
                .unwrap_or_else(|| self.fresh_ty());

            if let Some(sub_pat) = &fp.pattern {
                bindings.extend(self.infer_pattern(sub_pat, field_ty)?);
            } else {
                bindings.push((fp.name.clone(), field_ty));
            }
        }

        if let Some(Some(rest_name)) = &rp.rest {
            if let RowTail::Open(v) = tail {
                bindings.push((rest_name.clone(), Ty::Record(Row {
                    fields: vec![],
                    tail: RowTail::Open(v),
                })));
            }
        }

        Ok(bindings)
    }

    fn check_list_pattern(
        &mut self, lp: &ListPattern, expected: Ty,
    ) -> Result<Vec<(String, Ty)>, TypeError> {
        let elem = self.fresh_ty();
        self.unify(expected, Ty::List(Box::new(elem.clone())))?;
        let elem = self.subst.apply(&elem);

        let mut bindings = Vec::new();
        for p in &lp.elements {
            let elem_c = self.subst.apply(&elem);
            bindings.extend(self.infer_pattern(p, elem_c)?);
        }
        if let Some(Some(rest)) = &lp.rest {
            bindings.push((rest.clone(), Ty::List(Box::new(self.subst.apply(&elem)))));
        }
        Ok(bindings)
    }

    // ── Program-level checking ────────────────────────────────────────────────

    /// Extract the type of `field` from a (possibly-open) record type.
    fn extract_field_ty(&self, ty: &Ty, field: &str) -> Option<Ty> {
        if let Ty::Record(row) = ty {
            let row = self.subst.apply_row(row);
            row.fields.into_iter().find(|(k, _)| k == field).map(|(_, t)| t)
        } else {
            None
        }
    }

    /// Inject bindings from `uses` into `env`, loading modules via `loader`.
    /// Does nothing if `base` is `None` (e.g. LSP / in-memory source).
    fn apply_imports(
        &mut self,
        uses: &[UseDecl],
        base: Option<&Path>,
        loader: &mut crate::loader::Loader,
        env: &mut TypeEnv,
    ) -> Result<(), TypeErrorAt> {
        let base = match base {
            Some(b) => b,
            None => return Ok(()),
        };
        for u in uses {
            let scheme = loader.load(&u.path, base)?;

            match &u.binding {
                UseBinding::Ident(name) => {
                    // Import the whole module as a record value.
                    let ty = self.instantiate(&scheme);
                    env.insert(name.clone(), Scheme::mono(ty));
                }
                UseBinding::Record(rp) => {
                    // Destructured import: give each field its own scheme so
                    // polymorphic functions remain usable polymorphically.
                    let module_ty = self.instantiate(&scheme);
                    let module_ty = self.subst.apply(&module_ty);
                    for f in &rp.fields {
                        let field_ty =
                            self.extract_field_ty(&module_ty, &f.name).ok_or_else(|| {
                                TypeErrorAt::new(
                                    TypeError::FieldNotFound {
                                        field: f.name.clone(),
                                        record_ty: module_ty.clone(),
                                    },
                                    crate::error::Span::default(),
                                )
                            })?;
                        let s = self.generalise(env, &field_ty);
                        env.insert(f.name.clone(), s);
                    }
                }
            }
        }
        Ok(())
    }

    /// Type-check a full program, returning the type of the export expression.
    pub fn check_program(
        &mut self,
        program: &Program,
        mut env: TypeEnv,
        base: Option<&Path>,
        loader: &mut crate::loader::Loader,
    ) -> Result<Ty, TypeErrorAt> {
        self.apply_imports(&program.uses, base, loader, &mut env)?;

        for item in &program.items {
            if let TopItem::Binding(binding) = item {
                env = self.check_binding(binding, env)?;
            }
        }

        let export_ty = self.infer(&env, &program.exports)?;
        Ok(self.subst.apply(&export_ty))
    }

    fn check_binding(&mut self, binding: &Binding, env: TypeEnv) -> Result<TypeEnv, TypeErrorAt> {
        let value_span = &binding.value.span;

        // For simple name bindings, add a fresh monomorphic placeholder *before*
        // checking the body to support self-recursion.
        let (rec_env, placeholder) = match &binding.pattern {
            Pattern::Ident(name) => {
                let ph = self.fresh_ty();
                let ext = env.extend_one(name.clone(), ph.clone());
                (ext, Some(ph))
            }
            _ => (env.clone(), None),
        };

        let scheme = if let Some(ann) = &binding.ty {
            let mut param_vars: HashMap<String, TyVar> = HashMap::new();
            let ann_ty = self.lower_ty(ann, &mut param_vars)
                .map_err(|e| TypeErrorAt::new(e, value_span.clone()))?;
            if let Some(ph) = placeholder {
                self.unify_at(ph, ann_ty.clone(), value_span)?;
            }
            self.check(&rec_env, &binding.value, ann_ty.clone())?;
            self.generalise(&env, &ann_ty)
        } else {
            let inferred = self.infer(&rec_env, &binding.value)?;
            let inferred = self.subst.apply(&inferred);
            if let Some(ph) = placeholder {
                let ph = self.subst.apply(&ph);
                self.unify_at(ph, inferred.clone(), value_span)?;
            }
            self.generalise(&env, &inferred)
        };

        let mut new_env = env;
        self.bind_pattern_scheme(&binding.pattern, scheme, &mut new_env)
            .map_err(|e| TypeErrorAt::new(e, value_span.clone()))?;
        Ok(new_env)
    }

    fn bind_pattern_scheme(
        &mut self,
        pat: &Pattern,
        scheme: Scheme,
        env: &mut TypeEnv,
    ) -> Result<(), TypeError> {
        match pat {
            Pattern::Ident(name) => { env.insert(name.clone(), scheme); }
            Pattern::Wildcard => {}
            _ => {
                let ty = self.instantiate(&scheme);
                let bindings = self.infer_pattern(pat, ty)?;
                for (name, t) in bindings {
                    let t = self.subst.apply(&t);
                    let s = self.generalise(env, &t);
                    env.insert(name, s);
                }
            }
        }
        Ok(())
    }
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Type-check a full program. Returns the inferred type of the module exports.
///
/// `path` is the file being checked; it is used to resolve relative `use`
/// paths.  Pass `None` when checking an in-memory buffer (imports are skipped).
pub fn check_program(program: &Program, path: Option<&Path>) -> Result<Ty, TypeErrorAt> {
    let (env, mut var_env) = builtin_env();
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::new(var_env);
    let mut loader = crate::loader::Loader::new();
    checker.check_program(program, env, path, &mut loader)
}

/// One binding entry in the elaborated output.
pub struct BindingInfo {
    pub name: String,
    pub scheme: Scheme,
}

/// Type-check a program and return the inferred scheme for every named
/// top-level binding (in source order) plus the type of the export expression.
///
/// `path` is the file being checked; it is used to resolve relative `use`
/// paths.  Pass `None` when checking an in-memory buffer (imports are skipped).
pub fn elaborate(
    program: &Program,
    path: Option<&Path>,
) -> Result<(Vec<BindingInfo>, Ty), TypeErrorAt> {
    let (env, mut var_env) = builtin_env();
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::new(var_env);
    let mut loader = crate::loader::Loader::new();
    let mut env = env;

    checker.apply_imports(&program.uses, path, &mut loader, &mut env)?;

    let mut bindings: Vec<BindingInfo> = Vec::new();

    for item in &program.items {
        if let TopItem::Binding(binding) = item {
            env = checker.check_binding(binding, env)?;
            collect_binding_info(&binding.pattern, &env, &checker, &mut bindings);
        }
    }

    let export_ty = checker.infer(&env, &program.exports)?;
    let export_ty = checker.subst.apply(&export_ty);
    Ok((bindings, export_ty))
}

/// Walk a binding pattern and push a `BindingInfo` for every name it binds.
fn collect_binding_info(
    pat: &Pattern,
    env: &TypeEnv,
    checker: &Checker,
    out: &mut Vec<BindingInfo>,
) {
    match pat {
        Pattern::Ident(name) => {
            if let Some(scheme) = env.lookup(name) {
                let scheme = checker.subst.apply_scheme(scheme);
                out.push(BindingInfo { name: name.clone(), scheme });
            }
        }
        Pattern::Record(rp) => {
            for fp in &rp.fields {
                let name = fp.pattern.as_ref()
                    .and_then(|p| if let Pattern::Ident(n) = p { Some(n) } else { None })
                    .unwrap_or(&fp.name);
                if let Some(scheme) = env.lookup(name) {
                    let scheme = checker.subst.apply_scheme(scheme);
                    out.push(BindingInfo { name: name.clone(), scheme });
                }
            }
        }
        _ => {}
    }
}
