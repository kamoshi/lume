use std::collections::{HashMap, HashSet};
use crate::ast::{
    self, Binding, BinOp, Expr, FieldPattern, Literal, ListPattern, MatchArm, Pattern,
    Program, RecordField, RecordPattern, TopItem, UnOp,
};
use crate::types::{
    free_row_vars, free_type_vars, unify, Row, RowTail, Scheme, Subst, Ty, TyVar, TypeError,
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
    // zip : List a -> List b -> List (a, b) — simplified to returning List of a for now
    // (Lume doesn't have tuple types in this spec so we'll skip zip or use a placeholder)

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
    ) -> Result<(Ty, Option<Vec<(String, Ty)>>), TypeError> {
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

    pub fn infer(&mut self, env: &TypeEnv, expr: &Expr) -> Result<Ty, TypeError> {
        match expr {
            Expr::Number(_) => Ok(Ty::Num),
            Expr::Text(_) => Ok(Ty::Text),
            Expr::Bool(_) => Ok(Ty::Bool),

            Expr::List(exprs) => {
                let elem = self.fresh_ty();
                for e in exprs {
                    let t = self.infer(env, e)?;
                    let t = self.subst.apply(&t);
                    let elem_c = self.subst.apply(&elem);
                    self.unify(t, elem_c)?;
                }
                Ok(Ty::List(Box::new(self.subst.apply(&elem))))
            }

            Expr::Ident(name) => match env.lookup(name) {
                Some(scheme) => Ok(self.instantiate(scheme)),
                None => Err(TypeError::UnboundVariable(name.clone())),
            },

            // TypeIdent never produced by the parser — treat as unit variant.
            Expr::TypeIdent(name) => {
                let (result_ty, _) = self.instantiate_variant(name)?;
                Ok(result_ty)
            }

            Expr::Variant { name, payload: None } => {
                let (result_ty, payload_ty) = self.instantiate_variant(name)?;
                if payload_ty.is_some() {
                    // Has a payload but none was given; return as a constructor function.
                    // Shape: payload_ty -> Con(...)
                    let fields = payload_ty.unwrap();
                    let row = Row { fields, tail: RowTail::Closed };
                    let result_ty_c = result_ty.clone();
                    Ok(Ty::Func(Box::new(Ty::Record(row)), Box::new(result_ty_c)))
                } else {
                    Ok(result_ty)
                }
            }

            Expr::Variant { name, payload: Some(payload_expr) } => {
                let (result_ty, payload_fields) = self.instantiate_variant(name)?;
                let fields = payload_fields
                    .ok_or_else(|| TypeError::UnboundVariant(format!("{} is a unit variant", name)))?;
                let expected = Ty::Record(Row { fields, tail: RowTail::Closed });
                self.check(env, payload_expr, expected)?;
                Ok(result_ty)
            }

            Expr::Record { base: None, fields, .. } => {
                self.infer_record(env, fields)
            }

            Expr::Record { base: Some(base), fields, .. } => {
                self.infer_record_update(env, base, fields)
            }

            Expr::FieldAccess { record, field } => {
                self.infer_field_access(env, record, field)
            }

            Expr::Lambda { param, body } => {
                let param_ty = self.fresh_ty();
                let bindings = self.infer_pattern(param, param_ty.clone())?;
                let new_env = env.extend_many(bindings);
                let body_ty = self.infer(&new_env, body)?;
                let param_ty = self.subst.apply(&param_ty);
                let body_ty = self.subst.apply(&body_ty);
                Ok(Ty::Func(Box::new(param_ty), Box::new(body_ty)))
            }

            Expr::Apply { func, arg } => {
                let func_ty = self.infer(env, func)?;
                let arg_ty = self.infer(env, arg)?;
                let ret = self.fresh_ty();
                let func_ty = self.subst.apply(&func_ty);
                let arg_ty = self.subst.apply(&arg_ty);
                let ret_c = ret.clone();
                self.unify(func_ty, Ty::Func(Box::new(arg_ty), Box::new(ret)))?;
                Ok(self.subst.apply(&ret_c))
            }

            Expr::If { cond, then_branch, else_branch } => {
                let cond_ty = self.infer(env, cond)?;
                let cond_ty = self.subst.apply(&cond_ty);
                self.unify(cond_ty, Ty::Bool)?;
                let then_ty = self.infer(env, then_branch)?;
                let else_ty = self.infer(env, else_branch)?;
                let then_ty = self.subst.apply(&then_ty);
                let else_ty = self.subst.apply(&else_ty);
                self.unify(then_ty.clone(), else_ty)?;
                Ok(self.subst.apply(&then_ty))
            }

            Expr::Binary { op, left, right } => {
                self.infer_binary(env, op, left, right)
            }

            Expr::Unary { op, operand } => match op {
                UnOp::Neg => {
                    let t = self.infer(env, operand)?;
                    let t = self.subst.apply(&t);
                    self.unify(t, Ty::Num)?;
                    Ok(Ty::Num)
                }
                UnOp::Not => {
                    let t = self.infer(env, operand)?;
                    let t = self.subst.apply(&t);
                    self.unify(t, Ty::Bool)?;
                    Ok(Ty::Bool)
                }
            },

            Expr::Match(arms) => {
                self.infer_match(env, arms)
            }
        }
    }

    fn infer_record(
        &mut self, env: &TypeEnv, fields: &[RecordField],
    ) -> Result<Ty, TypeError> {
        let mut row_fields: Vec<(String, Ty)> = Vec::new();
        for f in fields {
            let ty = if let Some(val) = &f.value {
                self.infer(env, val)?
            } else {
                match env.lookup(&f.name) {
                    Some(s) => self.instantiate(s),
                    None => return Err(TypeError::UnboundVariable(f.name.clone())),
                }
            };
            row_fields.push((f.name.clone(), self.subst.apply(&ty)));
        }
        row_fields.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Ty::Record(Row { fields: row_fields, tail: RowTail::Closed }))
    }

    fn infer_record_update(
        &mut self, env: &TypeEnv, base: &Expr, overrides: &[RecordField],
    ) -> Result<Ty, TypeError> {
        let base_ty = self.infer(env, base)?;
        let base_ty = self.subst.apply(&base_ty);

        let mut new_fields: Vec<(String, Ty)> = Vec::new();
        for f in overrides {
            let ty = if let Some(val) = &f.value {
                self.infer(env, val)?
            } else {
                match env.lookup(&f.name) {
                    Some(s) => self.instantiate(s),
                    None => return Err(TypeError::UnboundVariable(f.name.clone())),
                }
            };
            new_fields.push((f.name.clone(), self.subst.apply(&ty)));
        }
        new_fields.sort_by(|a, b| a.0.cmp(&b.0));

        // Unify base with an empty open row to capture all its fields into base_rv.
        // This allows adding new fields (extension), not just overriding existing ones.
        let base_rv = self.fresh_var();
        let open_base = Ty::Record(Row { fields: vec![], tail: RowTail::Open(base_rv) });
        self.unify(base_ty, open_base)?;

        // Result: override fields on top of whatever base had (via base_rv).
        Ok(Ty::Record(Row { fields: new_fields, tail: RowTail::Open(base_rv) }))
    }

    fn infer_field_access(
        &mut self, env: &TypeEnv, record_expr: &Expr, field: &str,
    ) -> Result<Ty, TypeError> {
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
            return Err(TypeError::FieldNotFound { field: field.to_string(), record_ty: rec_ty });
        }

        // Generic case: constrain the record to have this field.
        let field_ty = self.fresh_ty();
        let row_tail = self.fresh_var();
        let expected = Ty::Record(Row {
            fields: vec![(field.to_string(), field_ty.clone())],
            tail: RowTail::Open(row_tail),
        });
        self.unify(rec_ty, expected)?;
        Ok(self.subst.apply(&field_ty))
    }

    fn infer_binary(
        &mut self, env: &TypeEnv, op: &BinOp, left: &Expr, right: &Expr,
    ) -> Result<Ty, TypeError> {
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                let tl = self.infer(env, left)?;
                let tl = self.subst.apply(&tl);
                self.unify(tl, Ty::Num)?;
                let tr = self.infer(env, right)?;
                let tr = self.subst.apply(&tr);
                self.unify(tr, Ty::Num)?;
                Ok(Ty::Num)
            }
            BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => {
                let tl = self.infer(env, left)?;
                let tl = self.subst.apply(&tl);
                self.unify(tl, Ty::Num)?;
                let tr = self.infer(env, right)?;
                let tr = self.subst.apply(&tr);
                self.unify(tr, Ty::Num)?;
                Ok(Ty::Bool)
            }
            BinOp::Eq | BinOp::NotEq => {
                let tl = self.infer(env, left)?;
                let tr = self.infer(env, right)?;
                let tl = self.subst.apply(&tl);
                let tr = self.subst.apply(&tr);
                self.unify(tl, tr)?;
                Ok(Ty::Bool)
            }
            BinOp::And | BinOp::Or => {
                let tl = self.infer(env, left)?;
                let tl = self.subst.apply(&tl);
                self.unify(tl, Ty::Bool)?;
                let tr = self.infer(env, right)?;
                let tr = self.subst.apply(&tr);
                self.unify(tr, Ty::Bool)?;
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
                self.unify(t_func, Ty::Func(Box::new(t_arg), Box::new(ret)))?;
                Ok(self.subst.apply(&ret_c))
            }
            BinOp::ResultPipe => self.infer_result_pipe(env, left, right),
        }
    }

    fn infer_concat(
        &mut self, env: &TypeEnv, left: &Expr, right: &Expr,
    ) -> Result<Ty, TypeError> {
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
                    // Simplified: return a fresh open record (proper merge would need row concat)
                    let rv = self.fresh_var();
                    Ok(Ty::Record(Row { fields: vec![], tail: RowTail::Open(rv) }))
                } else {
                    Err(TypeError::Mismatch(tl, tr))
                }
            }
            Ty::Var(_) => {
                // Unknown: infer right and unify
                let tr = self.infer(env, right)?;
                let tr = self.subst.apply(&tr);
                let tl = self.subst.apply(&tl);
                self.unify(tl, tr.clone())?;
                Ok(self.subst.apply(&tr))
            }
            t => Err(TypeError::ConcatNonConcatenable(t.clone())),
        }
    }

    fn infer_result_pipe(
        &mut self, env: &TypeEnv, left: &Expr, right: &Expr,
    ) -> Result<Ty, TypeError> {
        // left : Result a e ;  right : a -> Result b e ;  result : Result b e
        let tl = self.infer(env, left)?;
        let tl = self.subst.apply(&tl);
        let a = self.fresh_ty();
        let e = self.fresh_ty();
        let b = self.fresh_ty();
        let tl_c = tl.clone();
        self.unify(tl, Ty::Con("Result".into(), vec![a.clone(), e.clone()]))
            .map_err(|_| TypeError::ResultPipeNonResult(tl_c))?;
        let tr = self.infer(env, right)?;
        let tr = self.subst.apply(&tr);
        let a = self.subst.apply(&a);
        let e = self.subst.apply(&e);
        let b_c = b.clone();
        self.unify(tr, Ty::Func(
            Box::new(a),
            Box::new(Ty::Con("Result".into(), vec![b, e.clone()]))))?;
        let b = self.subst.apply(&b_c);
        let e = self.subst.apply(&e);
        Ok(Ty::Con("Result".into(), vec![b, e]))
    }

    fn infer_match(&mut self, env: &TypeEnv, arms: &[MatchArm]) -> Result<Ty, TypeError> {
        let t_in = self.fresh_ty();
        let t_out = self.fresh_ty();

        for arm in arms {
            let t_in_c = self.subst.apply(&t_in);
            let bindings = self.infer_pattern(&arm.pattern, t_in_c)?;
            let arm_env = env.extend_many(bindings);

            if let Some(guard) = &arm.guard {
                let tg = self.infer(&arm_env, guard)?;
                let tg = self.subst.apply(&tg);
                let tg_c = tg.clone();
                self.unify(tg, Ty::Bool).map_err(|_| TypeError::GuardNotBool(tg_c))?;
            }

            let t_body = self.infer(&arm_env, &arm.body)?;
            let t_body = self.subst.apply(&t_body);
            let t_out_c = self.subst.apply(&t_out);
            self.unify(t_body, t_out_c)?;
        }

        let t_in = self.subst.apply(&t_in);
        let t_out = self.subst.apply(&t_out);
        Ok(Ty::Func(Box::new(t_in), Box::new(t_out)))
    }

    // ── Check mode (⇐) ───────────────────────────────────────────────────────

    /// Check `expr` against `expected`, using bidirectional rules where possible.
    pub fn check(&mut self, env: &TypeEnv, expr: &Expr, expected: Ty) -> Result<(), TypeError> {
        let expected = self.subst.apply(&expected);
        match expr {
            // ── Lambda in check mode: propagate param type directly ──────────
            Expr::Lambda { param, body } => {
                if let Ty::Func(t_param, t_ret) = expected {
                    let bindings = self.infer_pattern(param, *t_param)?;
                    let new_env = env.extend_many(bindings);
                    self.check(&new_env, body, *t_ret)
                } else {
                    let inferred = self.infer(env, expr)?;
                    let inferred = self.subst.apply(&inferred);
                    self.unify(inferred, expected)
                }
            }
            // All other forms: infer + unify.
            _ => {
                let inferred = self.infer(env, expr)?;
                let inferred = self.subst.apply(&inferred);
                self.unify(inferred, expected)
            }
        }
    }

    // ── Pattern checking ─────────────────────────────────────────────────────

    /// Check `pat` against `expected`; returns (name, type) bindings.
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
        // Unify the variant's type with expected.
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
        // Assign a fresh type to each named field.
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

        // Bind the rest name if present.
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

    /// Type-check a full program, returning the type of the export expression.
    pub fn check_program(
        &mut self,
        program: &Program,
        mut env: TypeEnv,
    ) -> Result<Ty, TypeError> {
        for item in &program.items {
            if let TopItem::Binding(binding) = item {
                env = self.check_binding(binding, env)?;
            }
            // TypeDef items are handled by build_variant_env, not here.
        }

        let export_ty = self.infer(&env, &program.exports)?;
        Ok(self.subst.apply(&export_ty))
    }

    fn check_binding(&mut self, binding: &Binding, env: TypeEnv) -> Result<TypeEnv, TypeError> {
        // For simple name bindings, add a fresh monomorphic placeholder *before*
        // checking the body.  This lets the body refer to the binding's own name
        // (self-recursion) without an "unbound variable" error.
        //
        // The placeholder starts unconstrained (`?v`).  Any recursive call inside
        // the body will unify `?v` with whatever type is required, so after
        // inference the placeholder is already resolved to the correct type.
        let (rec_env, placeholder) = match &binding.pattern {
            Pattern::Ident(name) => {
                let ph = self.fresh_ty();
                let ext = env.extend_one(name.clone(), ph.clone());
                (ext, Some(ph))
            }
            _ => (env.clone(), None),
        };

        let scheme = if let Some(ann) = &binding.ty {
            // Annotation present → unify the placeholder with the annotation so
            // recursive calls inside the body see the declared type, then check.
            let mut param_vars: HashMap<String, TyVar> = HashMap::new();
            let ann_ty = self.lower_ty(ann, &mut param_vars)?;
            if let Some(ph) = placeholder {
                self.unify(ph, ann_ty.clone())?;
            }
            self.check(&rec_env, &binding.value, ann_ty.clone())?;
            self.generalise(&env, &ann_ty)
        } else {
            // No annotation → infer, then unify the placeholder with the result
            // to propagate any constraints from recursive calls.
            let inferred = self.infer(&rec_env, &binding.value)?;
            let inferred = self.subst.apply(&inferred);
            if let Some(ph) = placeholder {
                let ph = self.subst.apply(&ph);
                self.unify(ph, inferred.clone())?;
            }
            self.generalise(&env, &inferred)
        };

        // Bind the pattern in the env with the generalised scheme.
        let mut new_env = env;
        self.bind_pattern_scheme(&binding.pattern, scheme, &mut new_env)?;
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
                // Complex top-level pattern: instantiate and pattern-check.
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
pub fn check_program(program: &Program) -> Result<Ty, TypeError> {
    let (env, mut var_env) = builtin_env();
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::new(var_env);
    checker.check_program(program, env)
}

/// One binding entry in the elaborated output.
pub struct BindingInfo {
    pub name: String,
    pub scheme: Scheme,
}

/// Type-check a program and return the inferred scheme for every named
/// top-level binding (in source order) plus the type of the export expression.
pub fn elaborate(program: &Program) -> Result<(Vec<BindingInfo>, Ty), TypeError> {
    let (env, mut var_env) = builtin_env();
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::new(var_env);
    let mut env = env;

    let mut bindings: Vec<BindingInfo> = Vec::new();

    for item in &program.items {
        if let TopItem::Binding(binding) = item {
            env = checker.check_binding(binding, env)?;
            // After check_binding the scheme is now in env; snapshot it.
            collect_binding_info(&binding.pattern, &env, &checker, &mut bindings);
        }
        // TypeDef items don't produce value bindings, so nothing to collect.
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
                out.push(BindingInfo {
                    name: name.clone(),
                    scheme: checker.subst.apply_scheme(scheme),
                });
            }
        }
        Pattern::Record(rp) => {
            for fp in &rp.fields {
                let field_name = fp.pattern.as_ref()
                    .and_then(|p| if let Pattern::Ident(n) = p { Some(n.clone()) } else { None })
                    .unwrap_or_else(|| fp.name.clone());
                if let Some(scheme) = env.lookup(&field_name) {
                    out.push(BindingInfo {
                        name: field_name,
                        scheme: checker.subst.apply_scheme(scheme),
                    });
                }
            }
        }
        _ => {}
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser;

    fn lex_and_parse(src: &str) -> crate::ast::Program {
        let tokens = Lexer::new(src).tokenize().expect("lex error");
        parser::parse_program(&tokens).expect("parse error")
    }

    fn tc(src: &str) -> Result<Ty, TypeError> {
        let program = lex_and_parse(src);
        check_program(&program)
    }

    fn tc_expr(src: &str) -> Result<Ty, TypeError> {
        // Wrap in a minimal program: just the expression as the export.
        tc(src)
    }

    // ── Literals ──────────────────────────────────────────────────────────────

    #[test]
    fn tc_number() { assert_eq!(tc("42").unwrap(), Ty::Num); }

    #[test]
    fn tc_text() { assert_eq!(tc(r#""hello""#).unwrap(), Ty::Text); }

    #[test]
    fn tc_bool() { assert_eq!(tc("true").unwrap(), Ty::Bool); }

    #[test]
    fn tc_list_num() {
        let t = tc("[1, 2, 3]").unwrap();
        assert_eq!(t, Ty::List(Box::new(Ty::Num)));
    }

    #[test]
    fn tc_empty_list_is_polymorphic() {
        // []  should be  List ?N  for some fresh var
        let t = tc("[]").unwrap();
        assert!(matches!(t, Ty::List(_)));
    }

    // ── Arithmetic ────────────────────────────────────────────────────────────

    #[test]
    fn tc_add() {
        let t = tc("1 + 2").unwrap();
        assert_eq!(t, Ty::Num);
    }

    #[test]
    fn tc_comparison() {
        let t = tc("1 < 2").unwrap();
        assert_eq!(t, Ty::Bool);
    }

    #[test]
    fn tc_concat_text() {
        let t = tc(r#""hello" ++ " world""#).unwrap();
        assert_eq!(t, Ty::Text);
    }

    #[test]
    fn tc_concat_list() {
        let t = tc("[1, 2] ++ [3, 4]").unwrap();
        assert_eq!(t, Ty::List(Box::new(Ty::Num)));
    }

    // ── Lambda ────────────────────────────────────────────────────────────────

    #[test]
    fn tc_lambda_identity() {
        // n -> n  :  ?a -> ?a (polymorphic after generalisation)
        let t = tc("n -> n").unwrap();
        assert!(matches!(t, Ty::Func(..)));
    }

    #[test]
    fn tc_lambda_num() {
        let t = tc("n -> n * 2").unwrap();
        assert_eq!(t, Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num)));
    }

    // ── If expression ─────────────────────────────────────────────────────────

    #[test]
    fn tc_if() {
        let t = tc("if true then 1 else 0").unwrap();
        assert_eq!(t, Ty::Num);
    }

    #[test]
    fn tc_if_branch_mismatch() {
        assert!(tc(r#"if true then 1 else "hello""#).is_err());
    }

    // ── Let bindings ──────────────────────────────────────────────────────────

    #[test]
    fn tc_let_simple() {
        // let x = 42  ; export x
        let t = tc("let x = 42\nx").unwrap();
        assert_eq!(t, Ty::Num);
    }

    #[test]
    fn tc_let_fn() {
        let t = tc("let double = n -> n * 2\ndouble").unwrap();
        assert_eq!(t, Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num)));
    }

    #[test]
    fn tc_let_polymorphic_id() {
        // id applied at two different types — tests let generalisation.
        let t = tc("let id = x -> x\n[id 42, id 99]").unwrap();
        assert_eq!(t, Ty::List(Box::new(Ty::Num)));
    }

    // ── Function application ──────────────────────────────────────────────────

    #[test]
    fn tc_apply() {
        let t = tc("let double = n -> n * 2\ndouble 5").unwrap();
        assert_eq!(t, Ty::Num);
    }

    #[test]
    fn tc_apply_wrong_arg() {
        assert!(tc("let double = n -> n * 2\ndouble \"hello\"").is_err());
    }

    // ── Pipe ──────────────────────────────────────────────────────────────────

    #[test]
    fn tc_pipe() {
        let t = tc("let double = n -> n * 2\n5 |> double").unwrap();
        assert_eq!(t, Ty::Num);
    }

    // ── Records ───────────────────────────────────────────────────────────────

    #[test]
    fn tc_record_literal() {
        let tokens = crate::lexer::Lexer::new(r#"{ name: "Alice", age: 30 }"#)
            .tokenize().unwrap();
        let (_, expr) = crate::parser::parse_expr(&tokens).unwrap();
        let (env, var_env) = builtin_env();
        let mut checker = Checker::new(var_env);
        let t = checker.infer(&env, &expr).unwrap();
        let t = checker.subst.apply(&t);
        if let Ty::Record(row) = t {
            let fields: Vec<_> = row.fields.iter().map(|(k, _)| k.as_str()).collect();
            assert!(fields.contains(&"name"));
            assert!(fields.contains(&"age"));
        } else {
            panic!("expected Record, got something else");
        }
    }

    #[test]
    fn tc_field_access() {
        let t = tc("let alice = { name: \"Alice\", age: 30 }\nalice.age").unwrap();
        assert_eq!(t, Ty::Num);
    }

    #[test]
    fn tc_field_not_found() {
        assert!(tc("let r = { age: 30 }\nr.name").is_err());
    }

    // ── Row polymorphism ──────────────────────────────────────────────────────

    #[test]
    fn tc_row_poly_function() {
        // getName accepts any record with at least a `name: Text` field.
        let t = tc(
            r#"let getName = { name, .. } -> name
let alice = { name: "Alice", age: 30 }
getName alice"#
        ).unwrap();
        assert_eq!(t, Ty::Text);
    }

    #[test]
    fn tc_row_poly_missing_field() {
        assert!(tc(
            r#"let getName = { name, .. } -> name
let r = { age: 30 }
getName r"#
        ).is_err());
    }

    // ── Sum types ─────────────────────────────────────────────────────────────

    #[test]
    fn tc_sum_type_unit_variant() {
        let t = tc("type Direction = | North | South | East | West\nlet x = 42\nNorth").unwrap();
        assert_eq!(t, Ty::Con("Direction".into(), vec![]));
    }

    #[test]
    fn tc_sum_type_with_payload() {
        let t = tc(
            "type Shape = | Circle { radius: Num } | Rect { width: Num, height: Num }\nlet x = 42\nCircle { radius: 5 }"
        ).unwrap();
        assert_eq!(t, Ty::Con("Shape".into(), vec![]));
    }

    #[test]
    fn tc_generic_sum_type() {
        let t = tc("type Maybe a = | Some { value: a } | None\nlet x = 42\nSome { value: 42 }").unwrap();
        assert_eq!(t, Ty::Con("Maybe".into(), vec![Ty::Num]));
    }

    // ── Match expressions ─────────────────────────────────────────────────────

    #[test]
    fn tc_match_as_function() {
        let t = tc(
            r#"let x = 42
| 0 -> "zero"
| _ -> "other""#
        );
        // The match expression should typecheck; the program export is the match function.
        // If it doesn't error, we're good.
        assert!(t.is_ok(), "got error: {:?}", t);
        let t = t.unwrap();
        assert!(matches!(t, Ty::Func(..)));
    }

    #[test]
    fn tc_match_guard() {
        let t = tc(
            r#"let x = 42
let f =
  | n if n > 0 -> "positive"
  | _ -> "other"
f"#
        ).unwrap();
        assert_eq!(t, Ty::Func(Box::new(Ty::Num), Box::new(Ty::Text)));
    }

    // ── Type annotation ───────────────────────────────────────────────────────

    #[test]
    fn tc_annotation_ok() {
        let t = tc("let double : Num -> Num = n -> n * 2\ndouble").unwrap();
        assert_eq!(t, Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num)));
    }

    #[test]
    fn tc_annotation_wrong() {
        assert!(tc("let bad : Num -> Text = n -> n * 2\nbad").is_err());
    }

    // ── Boolean logic ─────────────────────────────────────────────────────────

    #[test]
    fn tc_and_or() {
        assert_eq!(tc("true and false").unwrap(), Ty::Bool);
        assert_eq!(tc("true or false").unwrap(), Ty::Bool);
    }

    #[test]
    fn tc_not() {
        assert_eq!(tc("not true").unwrap(), Ty::Bool);
    }

    // ── Recursive let bindings ────────────────────────────────────────────────

    #[test]
    fn tc_self_recursive_list() {
        // safeLast : List a -> Maybe a  — recurses on its own name
        let t = tc(
            "let safeLast =\n  | []         -> None\n  | [x]        -> Some { value: x }\n  | [_, ..rest] -> safeLast rest\nsafeLast"
        ).unwrap();
        // Should be a function  List ?a -> Maybe ?a
        assert!(matches!(t, Ty::Func(..)));
        if let Ty::Func(arg, ret) = &t {
            assert!(matches!(arg.as_ref(), Ty::List(_)));
            assert!(matches!(ret.as_ref(), Ty::Con(n, _) if n == "Maybe"));
        }
    }

    #[test]
    fn tc_self_recursive_counter() {
        // sum_to n = if n <= 0 then 0 else n + sum_to (n - 1)
        let t = tc(
            "let sumTo = n -> if n <= 0 then 0 else n + sumTo (n - 1)\nsumTo"
        ).unwrap();
        assert_eq!(t, Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num)));
    }

    #[test]
    fn tc_recursive_with_annotation() {
        let t = tc(
            "let sumTo : Num -> Num = n -> if n <= 0 then 0 else n + sumTo (n - 1)\nsumTo"
        ).unwrap();
        assert_eq!(t, Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num)));
    }

    #[test]
    fn tc_recursive_wrong_type_caught() {
        // Returning a Bool in a Num -> Num recursive function should fail.
        assert!(tc("let f : Num -> Num = n -> if n == 0 then true else f (n - 1)\nf").is_err());
    }
}
