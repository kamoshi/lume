use crate::ast::{
    self, BinOp, Binding, Expr, ExprKind, ListPattern, Literal, MatchArm, NodeId, Pattern, Program,
    RecordField, RecordPattern, TopItem, UnOp, UseBinding, UseDecl,
};
use crate::error::Span;
use crate::types::{
    free_row_vars, free_type_vars, unify, Row, RowTail, Scheme, Subst, Ty, TyVar, TypeError,
    TypeErrorAt,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

// ── Type environment ──────────────────────────────────────────────────────────

// The typing environment Γ maps source-level names to their Schemes.
//
// It is passed *immutably* through most of inference and cloned when a new
// scope is entered (let-binding, lambda parameter, match arm).  This gives us
// a purely-functional snapshot of the environment at each program point
// without needing a stack of undo operations.
//
// Cloning is cheap because Scheme is reference-counted internally via String
// and Vec, and environments are typically small (dozens of bindings).
#[derive(Debug, Clone, Default)]
pub struct TypeEnv(HashMap<String, Scheme>);

impl TypeEnv {
    pub fn new() -> Self {
        Self::default()
    }

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

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Scheme)> {
        self.0.iter()
    }

    // Compute the free variables of every scheme in the environment, taking
    // the current substitution into account.
    //
    // This is used by `generalise` to decide which variables are *not* safe
    // to quantify: any variable that is still free in the environment is
    // "in use" at the current let-binding level and must remain monomorphic.
    // Only variables that are free in the type being generalised but NOT in
    // the environment can be safely quantified.
    fn free_vars(&self, s: &Subst) -> (HashSet<TyVar>, HashSet<TyVar>) {
        let mut tvs = HashSet::new();
        let mut rvs = HashSet::new();
        for scheme in self.0.values() {
            let scheme = s.apply_scheme(scheme);
            let quant_t: HashSet<TyVar> = scheme.vars.iter().copied().collect();
            let quant_r: HashSet<TyVar> = scheme.row_vars.iter().copied().collect();
            for v in free_type_vars(&scheme.ty).difference(&quant_t) {
                tvs.insert(*v);
            }
            for v in free_row_vars(&scheme.ty).difference(&quant_r) {
                rvs.insert(*v);
            }
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
    pub fn lookup(&self, name: &str) -> Option<&VariantInfo> {
        self.0.get(name)
    }
    pub fn insert(&mut self, name: String, info: VariantInfo) {
        self.0.insert(name, info);
    }
    pub fn merge(&mut self, other: VariantEnv) {
        for (k, v) in other.0 {
            self.0.insert(k, v);
        }
    }
}

// ── Build variant environment from type definitions ───────────────────────────

// Scan the top-level items of a parsed program and register every variant
// constructor into a VariantEnv.  This pre-pass runs before type inference so
// that constructors like `Some`, `Ok`, `Node` etc. are available everywhere
// in the module, regardless of definition order.
//
// The stored payload uses AST types (not internal Ty), which means the fields
// are lowered fresh on every instantiation of the constructor.  This is
// intentional: each use site gets independent fresh type variables for the
// constructor's type parameters.
pub fn build_variant_env(items: &[TopItem]) -> VariantEnv {
    let mut env = VariantEnv::default();
    for item in items {
        if let TopItem::TypeDef(td) = item {
            for variant in &td.variants {
                let payload = variant.payload.as_ref().map(|rt| {
                    rt.fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty.clone()))
                        .collect()
                });
                env.insert(
                    variant.name.clone(),
                    VariantInfo {
                        type_name: td.name.clone(),
                        type_params: td.params.clone(),
                        payload_fields: payload,
                    },
                );
            }
        }
    }
    env
}

// ── Built-in environment ──────────────────────────────────────────────────────

// Produce the initial TypeEnv and VariantEnv containing all language-level
// built-ins (arithmetic, list ops, Maybe/Result constructors, etc.).
//
// WHY this takes `&mut Subst`:
//   Each type scheme that needs polymorphic variables (e.g. `map`, `fold`)
//   must use variable IDs that are guaranteed not to clash with any variable
//   the caller's type-checker will later allocate.  We achieve this by
//   drawing the IDs from the same shared counter via `s.fresh_var()`.
//   After `builtin_env` returns, the counter is past every ID it used, so the
//   caller's subsequent `fresh_var()` calls produce strictly larger IDs.
//
//   If we used hardcoded IDs (0, 1, 2 …) the ids would overlap with the
//   first variables the module's type-checker allocates, creating false
//   bindings when `instantiate`'s renaming substitution is applied.
pub fn builtin_env(s: &mut Subst) -> (TypeEnv, VariantEnv) {
    let mut env = TypeEnv::new();
    let mut var_env = VariantEnv::default();

    // Helper to create a scheme with N quantified type vars and 0 row vars.
    let mk_scheme = |vars: Vec<TyVar>, ty: Ty| Scheme {
        vars,
        row_vars: vec![],
        ty,
    };

    let v0 = s.fresh_var();
    let v1 = s.fresh_var();

    // Basic functions
    env.insert(
        "show".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(Box::new(Ty::Var(v0)), Box::new(Ty::Text)),
        ),
    );
    env.insert(
        "not".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Bool), Box::new(Ty::Bool))),
    );
    env.insert(
        "abs".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))),
    );
    env.insert(
        "round".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))),
    );
    env.insert(
        "floor".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))),
    );
    env.insert(
        "ceil".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))),
    );

    // Num -> Num -> Num
    let num2 = Ty::Func(
        Box::new(Ty::Num),
        Box::new(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Num))),
    );
    env.insert("max".into(), Scheme::mono(num2.clone()));
    env.insert("min".into(), Scheme::mono(num2.clone()));
    env.insert("mod".into(), Scheme::mono(num2.clone()));
    env.insert("pow".into(), Scheme::mono(num2));

    // toNum : Text -> Maybe Num
    env.insert(
        "toNum".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::Text),
            Box::new(Ty::Con("Maybe".into(), vec![Ty::Num])),
        )),
    );

    // range : Num -> Num -> List Num
    env.insert(
        "range".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::Num),
            Box::new(Ty::Func(
                Box::new(Ty::Num),
                Box::new(Ty::List(Box::new(Ty::Num))),
            )),
        )),
    );

    // List functions - use vars v0, v1
    let list_a = Ty::List(Box::new(Ty::Var(v0)));
    let list_b = Ty::List(Box::new(Ty::Var(v1)));

    // map : (a -> b) -> List a -> List b
    env.insert(
        "map".into(),
        mk_scheme(
            vec![v0, v1],
            Ty::Func(
                Box::new(Ty::Func(Box::new(Ty::Var(v0)), Box::new(Ty::Var(v1)))),
                Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_b.clone()))),
            ),
        ),
    );

    // filter : (a -> Bool) -> List a -> List a
    env.insert(
        "filter".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(
                Box::new(Ty::Func(Box::new(Ty::Var(v0)), Box::new(Ty::Bool))),
                Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone()))),
            ),
        ),
    );

    // fold : b -> (b -> a -> b) -> List a -> b
    env.insert(
        "fold".into(),
        mk_scheme(
            vec![v0, v1],
            Ty::Func(
                Box::new(Ty::Var(v1)),
                Box::new(Ty::Func(
                    Box::new(Ty::Func(
                        Box::new(Ty::Var(v1)),
                        Box::new(Ty::Func(Box::new(Ty::Var(v0)), Box::new(Ty::Var(v1)))),
                    )),
                    Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(Ty::Var(v1)))),
                )),
            ),
        ),
    );

    // length : List a -> Num
    env.insert(
        "length".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(Box::new(list_a.clone()), Box::new(Ty::Num)),
        ),
    );
    // reverse, sort : List a -> List a
    env.insert(
        "reverse".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone())),
        ),
    );
    env.insert(
        "sort".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::List(Box::new(Ty::Num))),
            Box::new(Ty::List(Box::new(Ty::Num))),
        )),
    );
    // take, drop : Num -> List a -> List a
    env.insert(
        "take".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(
                Box::new(Ty::Num),
                Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone()))),
            ),
        ),
    );
    env.insert(
        "drop".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(
                Box::new(Ty::Num),
                Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone()))),
            ),
        ),
    );
    // any, all : (a -> Bool) -> List a -> Bool
    let pred_a = Ty::Func(Box::new(Ty::Var(v0)), Box::new(Ty::Bool));
    env.insert(
        "any".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(
                Box::new(pred_a.clone()),
                Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(Ty::Bool))),
            ),
        ),
    );
    env.insert(
        "all".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(
                Box::new(pred_a.clone()),
                Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(Ty::Bool))),
            ),
        ),
    );
    // average, sum : List Num -> Num
    let list_num = Ty::List(Box::new(Ty::Num));
    env.insert(
        "average".into(),
        Scheme::mono(Ty::Func(Box::new(list_num.clone()), Box::new(Ty::Num))),
    );
    env.insert(
        "sum".into(),
        Scheme::mono(Ty::Func(Box::new(list_num.clone()), Box::new(Ty::Num))),
    );
    // sortBy : (a -> Num) -> List a -> List a
    env.insert(
        "sortBy".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(
                Box::new(Ty::Func(Box::new(Ty::Var(v0)), Box::new(Ty::Num))),
                Box::new(Ty::Func(Box::new(list_a.clone()), Box::new(list_a.clone()))),
            ),
        ),
    );

    // print : Text -> {}
    env.insert(
        "print".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::Text),
            Box::new(Ty::Record(Row {
                fields: vec![],
                tail: RowTail::Closed,
            })),
        )),
    );

    // readLine : {} -> Text
    let unit = Ty::Record(Row {
        fields: vec![],
        tail: RowTail::Closed,
    });
    env.insert(
        "readLine".into(),
        Scheme::mono(Ty::Func(Box::new(unit), Box::new(Ty::Text))),
    );

    // readFile : Text -> Result Text Text
    let result_text = Ty::Con("Result".into(), vec![Ty::Text, Ty::Text]);
    env.insert(
        "readFile".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Text), Box::new(result_text.clone()))),
    );

    // writeFile : Text -> Text -> Result {} Text
    let result_unit = Ty::Con(
        "Result".into(),
        vec![
            Ty::Record(Row {
                fields: vec![],
                tail: RowTail::Closed,
            }),
            Ty::Text,
        ],
    );
    env.insert(
        "writeFile".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::Text),
            Box::new(Ty::Func(Box::new(Ty::Text), Box::new(result_unit.clone()))),
        )),
    );

    // appendFile : Text -> Text -> Result {} Text
    env.insert(
        "appendFile".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::Text),
            Box::new(Ty::Func(Box::new(Ty::Text), Box::new(result_unit))),
        )),
    );

    // Text functions
    env.insert(
        "trim".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Text), Box::new(Ty::Text))),
    );
    env.insert(
        "toUpper".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Text), Box::new(Ty::Text))),
    );
    env.insert(
        "toLower".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Text), Box::new(Ty::Text))),
    );
    // split : Text -> Text -> List Text
    env.insert(
        "split".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::Text),
            Box::new(Ty::Func(
                Box::new(Ty::Text),
                Box::new(Ty::List(Box::new(Ty::Text))),
            )),
        )),
    );
    // join : Text -> List Text -> Text
    env.insert(
        "join".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::Text),
            Box::new(Ty::Func(
                Box::new(Ty::List(Box::new(Ty::Text))),
                Box::new(Ty::Text),
            )),
        )),
    );
    // contains, startsWith, endsWith : Text -> Text -> Bool
    let t2bool = Ty::Func(
        Box::new(Ty::Text),
        Box::new(Ty::Func(Box::new(Ty::Text), Box::new(Ty::Bool))),
    );
    env.insert("contains".into(), Scheme::mono(t2bool.clone()));
    env.insert("startsWith".into(), Scheme::mono(t2bool.clone()));
    env.insert("endsWith".into(), Scheme::mono(t2bool));

    // Result helpers: unwrap : Result a e -> a
    env.insert(
        "unwrap".into(),
        mk_scheme(
            vec![v0, v1],
            Ty::Func(
                Box::new(Ty::Con("Result".into(), vec![Ty::Var(v0), Ty::Var(v1)])),
                Box::new(Ty::Var(v0)),
            ),
        ),
    );
    // withDefault : a -> Maybe a -> a
    env.insert(
        "withDefault".into(),
        mk_scheme(
            vec![v0],
            Ty::Func(
                Box::new(Ty::Var(v0)),
                Box::new(Ty::Func(
                    Box::new(Ty::Con("Maybe".into(), vec![Ty::Var(v0)])),
                    Box::new(Ty::Var(v0)),
                )),
            ),
        ),
    );
    // mapErr : (e -> f) -> Result a e -> Result a f
    env.insert(
        "mapErr".into(),
        mk_scheme(
            vec![0, 1, 2],
            Ty::Func(
                Box::new(Ty::Func(Box::new(Ty::Var(1)), Box::new(Ty::Var(2)))),
                Box::new(Ty::Func(
                    Box::new(Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(1)])),
                    Box::new(Ty::Con("Result".into(), vec![Ty::Var(0), Ty::Var(2)])),
                )),
            ),
        ),
    );

    // Maybe variants: Some { value: a }, None
    var_env.insert(
        "Some".into(),
        VariantInfo {
            type_name: "Maybe".into(),
            type_params: vec!["a".into()],
            payload_fields: Some(vec![("value".into(), ast::Type::Var("a".into()))]),
        },
    );
    var_env.insert(
        "None".into(),
        VariantInfo {
            type_name: "Maybe".into(),
            type_params: vec!["a".into()],
            payload_fields: None,
        },
    );

    // Result variants: Ok { value: a }, Err { reason: b }
    var_env.insert(
        "Ok".into(),
        VariantInfo {
            type_name: "Result".into(),
            type_params: vec!["a".into(), "b".into()],
            payload_fields: Some(vec![("value".into(), ast::Type::Var("a".into()))]),
        },
    );
    var_env.insert(
        "Err".into(),
        VariantInfo {
            type_name: "Result".into(),
            type_params: vec!["a".into(), "b".into()],
            payload_fields: Some(vec![("reason".into(), ast::Type::Var("b".into()))]),
        },
    );

    (env, var_env)
}

/// Return type of `instantiate_variant`: (result_type, optional payload fields).
type VariantInstance = (Ty, Option<Vec<(String, Ty)>>);

// ── The typechecker ───────────────────────────────────────────────────────────

// Checker owns the mutable state for one type-checking session (one module).
//
// `subst`       - the live substitution; grows as constraints are solved.
// `variant_env` - constructor metadata; read-only after construction.
// `node_types`  - side channel: every AST node's inferred type, keyed by
//                 NodeId.  Used by the LSP for hover/completion information.
//                 Types here still contain unification variables; callers
//                 apply the final `subst` to obtain ground types.
pub struct Checker {
    pub subst: Subst,
    pub variant_env: VariantEnv,
    /// Maps each expression's NodeId to its inferred type (with type vars, resolved at end).
    pub node_types: HashMap<NodeId, Ty>,
}

impl Checker {
    pub fn new(variant_env: VariantEnv) -> Self {
        Checker {
            subst: Subst::new(),
            variant_env,
            node_types: HashMap::new(),
        }
    }

    // Construct a Checker that continues from an existing Subst (pre-populated
    // counter and bindings).  Used by `check_and_generalise` in the loader so
    // that the Subst started by `builtin_env` is handed directly to the
    // Checker rather than recreated, keeping variable IDs consistent.
    pub fn with_subst(variant_env: VariantEnv, subst: Subst) -> Self {
        Checker {
            subst,
            variant_env,
            node_types: HashMap::new(),
        }
    }

    fn fresh_var(&mut self) -> TyVar {
        self.subst.fresh_var()
    }
    fn fresh_ty(&mut self) -> Ty {
        self.subst.fresh_ty()
    }

    // ── Instantiation & generalisation ──────────────────────────────────────

    // Create a *fresh copy* of a polymorphic scheme for use at a single call
    // site (the HM "inst" rule).
    //
    // Every quantified variable in the scheme is replaced by a brand-new
    // unification variable drawn from the current Subst counter.  Different
    // call sites therefore get independent sets of variables that unify
    // independently - this is what makes a function like `id : a -> a`
    // usable at both `id 1` (a=Num) and `id true` (a=Bool) in the same scope.
    //
    // Implementation note - the counter-advance before allocation:
    //   The scheme's quantified var IDs are arbitrary integers (e.g. [3, 7]).
    //   If our counter happened to be at 3, the first fresh var would be
    //   Var(3), which equals the first quantified var.  The temporary Subst
    //   `tmp` would then map { 3 → Var(3) } - an identity - and when
    //   `tmp.apply` followed Var(3), it would chain into the *next* entry
    //   in the mapping, creating spurious connections between scheme vars.
    //
    //   The fix: advance the counter to max(scheme_vars)+1 before we draw any
    //   fresh vars.  This guarantees domain and range are disjoint, so
    //   `tmp.apply` is a true one-step renaming with no accidental chaining.
    fn instantiate(&mut self, scheme: &Scheme) -> Ty {
        if scheme.vars.is_empty() && scheme.row_vars.is_empty() {
            return scheme.ty.clone();
        }
        // Advance the counter past all quantified var IDs so that the fresh
        // vars we create are disjoint from the ones we're renaming.  Without
        // this, `tmp.apply` can follow a chain old→fresh→old (when a fresh var
        // happens to equal another quantified var), creating spurious cycles.
        let max_qv = scheme
            .vars
            .iter()
            .chain(scheme.row_vars.iter())
            .copied()
            .max()
            .unwrap_or(0);
        if self.subst.counter <= max_qv {
            self.subst.counter = max_qv + 1;
        }
        let mut local_tys: HashMap<TyVar, Ty> = HashMap::new();
        let mut local_rows: HashMap<TyVar, Row> = HashMap::new();
        for &v in &scheme.vars {
            local_tys.insert(v, self.fresh_ty());
        }
        for &v in &scheme.row_vars {
            // Row vars are instantiated as fresh *open* rows (no known fields
            // yet); unification will fill them in as needed.
            local_rows.insert(
                v,
                Row {
                    fields: vec![],
                    tail: RowTail::Open(self.fresh_var()),
                },
            );
        }
        // Build a temporary Subst containing *only* the renaming.  It does
        // not include the ambient `self.subst` bindings because the scheme's
        // type was already fully normalised by `generalise_toplevel` / the
        // caller; remaining Var nodes are exactly the quantified ones.
        let tmp = Subst {
            counter: self.subst.counter,
            tys: local_tys,
            rows: local_rows,
        };
        tmp.apply(&scheme.ty)
    }

    // Generalise a type into a scheme by quantifying the variables that are
    // free in `ty` but NOT free in `env` (the HM "gen" / "let" rule).
    //
    // The logic:
    //   • Apply the current substitution to `ty` to get a maximally-ground type.
    //   • Collect free type/row vars of that normalised type.
    //   • Subtract the free vars of the environment (those are monomorphic at
    //     this point: they appear in an outer lambda parameter or a recursive
    //     placeholder that hasn't been solved yet).
    //   • The remaining vars are truly generic - they can be safely quantified.
    //
    // Example: in `let id = fn x -> x`, after inferring `x : ?a`, the body
    // has type `?a`.  The env has `x : ?a` so ?a IS in the env's free vars.
    // But at the let-binding level, the *outer* env does not contain ?a, so
    // generalise over the outer env produces ∀ a. a.
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
        self.unify(t1, t2)
            .map_err(|e| TypeErrorAt::new(e, span.clone()))
    }

    // ── AST type lowering ────────────────────────────────────────────────────

    // Convert a surface-syntax type annotation into an internal `Ty`.
    //
    // `param_vars` is a mutable map from type-parameter *names* (the strings
    // written in source, e.g. "a", "b") to the fresh TyVar allocated for that
    // name.  The map is shared across all fields / nested types of the same
    // annotation so that the same name always maps to the same variable.
    // Example: `a -> Maybe a` correctly gets the same TyVar for both `a`s.
    fn lower_ty(
        &mut self,
        ty: &ast::Type,
        param_vars: &mut HashMap<String, TyVar>,
    ) -> Result<Ty, TypeError> {
        use ast::Type;
        match ty {
            Type::Named { name, args } => match name.as_str() {
                "Num" => Ok(Ty::Num),
                "Text" => Ok(Ty::Text),
                "Bool" => Ok(Ty::Bool),
                "List" if args.len() == 1 => {
                    let inner = self.lower_ty(&args[0], param_vars)?;
                    Ok(Ty::List(Box::new(inner)))
                }
                _ => {
                    let conv: Result<Vec<_>, _> =
                        args.iter().map(|a| self.lower_ty(a, param_vars)).collect();
                    Ok(Ty::Con(name.clone(), conv?))
                }
            },
            Type::Var(name) => {
                let v = param_vars
                    .entry(name.clone())
                    .or_insert_with(|| self.subst.fresh_var());
                Ok(Ty::Var(*v))
            }
            Type::Record(rt) => {
                let mut fields: Vec<(String, Ty)> = rt
                    .fields
                    .iter()
                    .map(|f| {
                        self.lower_ty(&f.ty, param_vars)
                            .map(|t| (f.name.clone(), t))
                    })
                    .collect::<Result<_, _>>()?;
                fields.sort_by(|a, b| a.0.cmp(&b.0));
                let tail = if rt.open {
                    RowTail::Open(self.fresh_var())
                } else {
                    RowTail::Closed
                };
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

    // Produce fresh types for one use of a variant constructor.
    //
    // Returns `(result_ty, payload_fields)` where:
    //   • `result_ty`      is the ADT type with fresh vars for each param,
    //                      e.g. `Maybe ?a` with a fresh ?a per call.
    //   • `payload_fields` is the typed record the constructor expects as its
    //                      argument, or None for unit constructors.
    //
    // The payload's AST types (stored in VariantInfo) are lowered through
    // `lower_ty` using the same `param_vars` map that was populated when
    // building `result_ty`.  This guarantees the param names unify correctly:
    // `Some { value: a }` gives a payload field `value : ?a` where ?a is the
    // same variable as in `Maybe ?a`.
    fn instantiate_variant(&mut self, name: &str) -> Result<VariantInstance, TypeError> {
        let info = self
            .variant_env
            .lookup(name)
            .ok_or_else(|| TypeError::UnboundVariant(name.to_string()))?
            .clone(); // clone to release the borrow before calling lower_ty

        // Map each type param name → fresh TyVar
        let mut param_vars: HashMap<String, TyVar> = HashMap::new();
        let fresh_args: Vec<Ty> = info
            .type_params
            .iter()
            .map(|p| {
                let v = self.fresh_var();
                param_vars.insert(p.clone(), v);
                Ty::Var(v)
            })
            .collect();

        let result_ty = Ty::Con(info.type_name.clone(), fresh_args);

        let payload_ty = if let Some(fields) = info.payload_fields {
            let mut converted: Vec<(String, Ty)> = fields
                .iter()
                .map(|(fname, fty)| {
                    self.lower_ty(fty, &mut param_vars)
                        .map(|t| (fname.clone(), t))
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

    // The top-level inference entry point.  Delegates to `infer_inner` for the
    // actual logic, then stores the type in `node_types` for the LSP.
    pub fn infer(&mut self, env: &TypeEnv, expr: &Expr) -> Result<Ty, TypeErrorAt> {
        let ty = self.infer_inner(env, expr)?;
        self.node_types.insert(expr.id, ty.clone());
        Ok(ty)
    }

    // Bidirectional type inference - the synthesis (⇒) direction.
    //
    // For each expression form we *produce* a type rather than checking against
    // a known one.  The dual direction (checking ⇐) is in `check` below.
    //
    // A recurring pattern throughout:
    //   1. Infer subexpression types.
    //   2. Apply the current substitution to them (`self.subst.apply`) to get
    //      the most-ground version before unification.  This is important: a
    //      variable that was solved by a previous step might appear in the type
    //      returned by an earlier `infer` call, and we must chase the chain to
    //      see whether it's already grounded.
    //   3. Unify subexpression types with whatever constraints the form imposes
    //      (e.g. both branches of an `if` must agree).
    //   4. Return the result type (also applied to get the latest ground form).
    fn infer_inner(&mut self, env: &TypeEnv, expr: &Expr) -> Result<Ty, TypeErrorAt> {
        let span = &expr.span;
        match &expr.kind {
            // Literal forms have fixed, known types - no constraints needed.
            ExprKind::Number(_) => Ok(Ty::Num),
            ExprKind::Text(_) => Ok(Ty::Text),
            ExprKind::Bool(_) => Ok(Ty::Bool),

            // List literal: all elements must share the same type.
            // We introduce a fresh element variable and unify each element's
            // type against it.  If the list is empty the element type stays
            // polymorphic (a free variable in the result).
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

            // Variable reference: look up the scheme and instantiate it.
            // Instantiation replaces quantified vars with fresh ones so that
            // this particular use of the variable gets its own type variables.
            ExprKind::Ident(name) => match env.lookup(name) {
                Some(scheme) => Ok(self.instantiate(scheme)),
                None => Err(TypeErrorAt::new(
                    TypeError::UnboundVariable(name.clone()),
                    span.clone(),
                )),
            },

            // Variant used without a payload (e.g. bare `None` or `Some` as
            // a first-class function).
            // If the variant *has* a payload type, treat the bare name as a
            // curried constructor: `{ payload_fields } -> ConType`.
            ExprKind::Variant {
                name,
                payload: None,
            } => {
                let (result_ty, payload_ty) = self
                    .instantiate_variant(name)
                    .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                if let Some(fields) = payload_ty {
                    // Has a payload but none was given; return as a constructor function.
                    let row = Row {
                        fields,
                        tail: RowTail::Closed,
                    };
                    let result_ty_c = result_ty.clone();
                    Ok(Ty::Func(Box::new(Ty::Record(row)), Box::new(result_ty_c)))
                } else {
                    Ok(result_ty)
                }
            }

            // Variant applied to a payload expression (e.g. `Some { value: x }`).
            // We check the payload expression against the expected record type
            // (check mode, not infer) because we know the exact shape.
            ExprKind::Variant {
                name,
                payload: Some(payload_expr),
            } => {
                let (result_ty, payload_fields) = self
                    .instantiate_variant(name)
                    .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                let fields = payload_fields.ok_or_else(|| {
                    TypeErrorAt::new(
                        TypeError::UnboundVariant(format!("{} is a unit variant", name)),
                        span.clone(),
                    )
                })?;
                let expected = Ty::Record(Row {
                    fields,
                    tail: RowTail::Closed,
                });
                self.check(env, payload_expr, expected)?;
                Ok(result_ty)
            }

            ExprKind::Record {
                base: None, fields, ..
            } => self.infer_record(env, fields, span),

            ExprKind::Record {
                base: Some(base),
                fields,
                ..
            } => self.infer_record_update(env, base, fields),

            ExprKind::FieldAccess { record, field } => self.infer_field_access(env, record, field),

            ExprKind::Lambda { param, body } => {
                let param_ty = self.fresh_ty();
                let bindings = self
                    .infer_pattern(param, param_ty.clone())
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
                self.unify_at(
                    func_ty,
                    Ty::Func(Box::new(arg_ty), Box::new(ret)),
                    &func.span,
                )?;
                Ok(self.subst.apply(&ret_c))
            }

            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
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

            ExprKind::Binary { op, left, right } => self.infer_binary(env, op, left, right),

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

            ExprKind::Match(arms) => self.infer_match(env, arms, span),

            ExprKind::LetIn {
                pattern,
                value,
                body,
            } => {
                let val_ty = self.infer(env, value)?;
                let bindings = self
                    .infer_pattern(pattern, val_ty)
                    .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                let new_env = env.extend_many(bindings);
                self.infer(&new_env, body)
            }
        }
    }

    fn infer_record(
        &mut self,
        env: &TypeEnv,
        fields: &[RecordField],
        span: &Span,
    ) -> Result<Ty, TypeErrorAt> {
        let mut row_fields: Vec<(String, Ty)> = Vec::new();
        for f in fields {
            let ty = if let Some(val) = &f.value {
                self.infer(env, val)?
            } else {
                match env.lookup(&f.name) {
                    Some(s) => self.instantiate(s),
                    None => {
                        return Err(TypeErrorAt::new(
                            TypeError::UnboundVariable(f.name.clone()),
                            span.clone(),
                        ))
                    }
                }
            };
            let ty = self.subst.apply(&ty);
            self.node_types.insert(f.name_node_id, ty.clone());
            row_fields.push((f.name.clone(), ty));
        }
        row_fields.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Ty::Record(Row {
            fields: row_fields,
            tail: RowTail::Closed,
        }))
    }

    // Record update syntax: `{ base | field: val, … }`.
    //
    // The result type keeps all fields of `base` and overlays the updated
    // fields.  We achieve this with a row variable trick:
    //
    //   1. Infer the types of the override fields normally.
    //   2. Unify `base` with an *empty open row* - this binds a fresh row var
    //      `base_rv` to "whatever fields base has".  After unification,
    //      `base_rv` points to the base record's full field set.
    //   3. Return a new record whose explicit fields are the overrides and
    //      whose tail is `Open(base_rv)`.  Row-polymorphism then ensures that
    //      downstream consumers see the union: the override fields shadow any
    //      same-named fields inherited from the base.
    fn infer_record_update(
        &mut self,
        env: &TypeEnv,
        base: &Expr,
        overrides: &[RecordField],
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
                    None => {
                        return Err(TypeErrorAt::new(
                            TypeError::UnboundVariable(f.name.clone()),
                            base.span.clone(),
                        ))
                    }
                }
            };
            let ty = self.subst.apply(&ty);
            self.node_types.insert(f.name_node_id, ty.clone());
            new_fields.push((f.name.clone(), ty));
        }
        new_fields.sort_by(|a, b| a.0.cmp(&b.0));

        // Unify base with an empty open row to capture all its fields into base_rv.
        let base_rv = self.fresh_var();
        let open_base = Ty::Record(Row {
            fields: vec![],
            tail: RowTail::Open(base_rv),
        });
        self.unify_at(base_ty, open_base, &base.span)?;

        Ok(Ty::Record(Row {
            fields: new_fields,
            tail: RowTail::Open(base_rv),
        }))
    }

    // Field access `expr.field`.
    //
    // Fast path: if we already know the full record type, look the field up
    // directly.  If the record is open and the field isn't found yet, extend
    // the open tail with a new field constraint.
    //
    // Generic path: if the expression's type is not yet known to be a record
    // (e.g. it's still a Var), unify it against a minimal one-field open
    // record.  This *adds* the field constraint to whatever the variable
    // eventually resolves to.
    fn infer_field_access(
        &mut self,
        env: &TypeEnv,
        record_expr: &Expr,
        field: &str,
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
            // The field is not present yet but the row is open - extend the
            // tail to include the missing field.
            if let RowTail::Open(v) = row.tail {
                let field_ty = self.fresh_ty();
                let rest_var = self.fresh_var();
                self.subst.bind_row(
                    v,
                    Row {
                        fields: vec![(field.to_string(), field_ty.clone())],
                        tail: RowTail::Open(rest_var),
                    },
                );
                return Ok(self.subst.apply(&field_ty));
            }
            return Err(TypeErrorAt::new(
                TypeError::FieldNotFound {
                    field: field.to_string(),
                    record_ty: rec_ty,
                },
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
        &mut self,
        env: &TypeEnv,
        op: &BinOp,
        left: &Expr,
        right: &Expr,
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
                self.unify_at(
                    t_func,
                    Ty::Func(Box::new(t_arg), Box::new(ret)),
                    &right.span,
                )?;
                Ok(self.subst.apply(&ret_c))
            }
            BinOp::ResultPipe => self.infer_result_pipe(env, left, right),
        }
    }

    fn infer_concat(
        &mut self,
        env: &TypeEnv,
        left: &Expr,
        right: &Expr,
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
                    Ok(Ty::Record(Row {
                        fields: vec![],
                        tail: RowTail::Open(rv),
                    }))
                } else {
                    Err(TypeErrorAt::new(
                        TypeError::Mismatch(tl, tr),
                        right.span.clone(),
                    ))
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
                TypeError::ConcatNonConcatenable(t.clone()),
                left.span.clone(),
            )),
        }
    }

    fn infer_result_pipe(
        &mut self,
        env: &TypeEnv,
        left: &Expr,
        right: &Expr,
    ) -> Result<Ty, TypeErrorAt> {
        // left : Result a e ;  right : a -> Result b e ;  result : Result b e
        let tl = self.infer(env, left)?;
        let tl = self.subst.apply(&tl);
        let a = self.fresh_ty();
        let e = self.fresh_ty();
        let b = self.fresh_ty();
        let tl_c = tl.clone();
        self.unify(tl, Ty::Con("Result".into(), vec![a.clone(), e.clone()]))
            .map_err(|_| {
                TypeErrorAt::new(TypeError::ResultPipeNonResult(tl_c), left.span.clone())
            })?;
        let tr = self.infer(env, right)?;
        let tr = self.subst.apply(&tr);
        let a = self.subst.apply(&a);
        let e = self.subst.apply(&e);
        let b_c = b.clone();
        self.unify_at(
            tr,
            Ty::Func(
                Box::new(a),
                Box::new(Ty::Con("Result".into(), vec![b, e.clone()])),
            ),
            &right.span,
        )?;
        let b = self.subst.apply(&b_c);
        let e = self.subst.apply(&e);
        Ok(Ty::Con("Result".into(), vec![b, e]))
    }

    // A top-level `| pat -> body` chain is treated as a lambda:
    //   • `t_in`  - the (unknown) argument type, shared across all arms.
    //   • `t_out` - the (unknown) result type, also shared.
    // Each arm narrows `t_in` by unifying the pattern's expected type against
    // it, and narrows `t_out` by unifying the body's inferred type against it.
    // The final type is `t_in -> t_out`.
    fn infer_match(
        &mut self,
        env: &TypeEnv,
        arms: &[MatchArm],
        span: &Span,
    ) -> Result<Ty, TypeErrorAt> {
        let t_in = self.fresh_ty();
        let t_out = self.fresh_ty();

        for arm in arms {
            // Re-apply t_in each iteration: earlier arms may have solved it.
            let t_in_c = self.subst.apply(&t_in);
            let bindings = self
                .infer_pattern(&arm.pattern, t_in_c)
                .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
            let arm_env = env.extend_many(bindings);

            if let Some(guard) = &arm.guard {
                let tg = self.infer(&arm_env, guard)?;
                let tg = self.subst.apply(&tg);
                let tg_c = tg.clone();
                self.unify(tg, Ty::Bool).map_err(|_| {
                    TypeErrorAt::new(TypeError::GuardNotBool(tg_c), guard.span.clone())
                })?;
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

    // The checking (⇐) direction: we know what type `expr` *must* have and
    // verify it against that expectation.
    //
    // This avoids introducing a unification variable for forms where we already
    // know the type - primarily lambdas.  For a lambda `fn x -> body` checked
    // against `A -> B`, we can directly bind the parameter `x : A` without
    // going through a fresh variable and a subsequent unification.
    //
    // For all other expression forms we fall back to synthesise-then-unify:
    // infer the expression's type and unify it with `expected`.
    pub fn check(&mut self, env: &TypeEnv, expr: &Expr, expected: Ty) -> Result<(), TypeErrorAt> {
        let span = &expr.span;
        let expected = self.subst.apply(&expected);
        match &expr.kind {
            // ── Lambda in check mode: propagate param type directly ──────────
            ExprKind::Lambda { param, body } => {
                if let Ty::Func(t_param, t_ret) = expected.clone() {
                    let bindings = self
                        .infer_pattern(param, *t_param)
                        .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                    let new_env = env.extend_many(bindings);
                    self.check(&new_env, body, *t_ret)?;
                    // Record the lambda's type (known from the checked expected type).
                    self.node_types.insert(expr.id, self.subst.apply(&expected));
                    Ok(())
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

    // Patterns do two things simultaneously:
    //  1. They ensure the shape of the data matches the `expected` type (by unifying).
    //  2. They return a list of local bindings (name -> type) introduced by the pattern.
    //
    // Pattern errors don't carry expression spans; callers convert as needed.
    pub fn infer_pattern(
        &mut self,
        pat: &Pattern,
        expected: Ty,
    ) -> Result<Vec<(String, Ty)>, TypeError> {
        let expected = self.subst.apply(&expected);
        match pat {
            Pattern::Wildcard => Ok(vec![]),

            Pattern::Literal(lit) => {
                let lit_ty = match lit {
                    Literal::Number(_) => Ty::Num,
                    Literal::Text(_) => Ty::Text,
                    Literal::Bool(_) => Ty::Bool,
                };
                self.unify(expected, lit_ty)?;
                Ok(vec![])
            }

            // The simplest binding: `x` simply takes on whatever the `expected` type is.
            Pattern::Ident(name, _, node_id) => {
                self.node_types.insert(*node_id, expected.clone());
                Ok(vec![(name.clone(), expected)])
            }

            Pattern::Variant { name, payload } => {
                self.check_variant_pattern(name, payload.as_deref(), expected)
            }

            Pattern::Record(rp) => self.check_record_pattern(rp, expected),

            Pattern::List(lp) => self.check_list_pattern(lp, expected),
        }
    }

    // Checking a record pattern: `{ x, y, ..rest }`
    //
    // We construct a fresh row type representing the fields we *know* about from the
    // pattern. If the pattern has `..rest`, the row tail is Open (a fresh variable),
    // meaning the actual record can have more fields. If it has no `..rest`, the tail
    // is Closed, meaning the expected record must have *exactly* these fields.
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
            (Some(_), None) => Ok(vec![]), // payload ignored
            (Some(fields), Some(p)) => {
                let payload_ty = Ty::Record(Row {
                    fields,
                    tail: RowTail::Closed,
                });
                self.infer_pattern(p, payload_ty)
            }
            (None, Some(p)) => {
                // Unit variant but pattern given - wildcard fallback
                self.infer_pattern(
                    p,
                    Ty::Record(Row {
                        fields: vec![],
                        tail: RowTail::Closed,
                    }),
                )
            }
        }
    }

    fn check_record_pattern(
        &mut self,
        rp: &RecordPattern,
        expected: Ty,
    ) -> Result<Vec<(String, Ty)>, TypeError> {
        let mut field_tys: Vec<(String, Ty)> = rp
            .fields
            .iter()
            .map(|fp| (fp.name.clone(), self.fresh_ty()))
            .collect();
        field_tys.sort_by(|a, b| a.0.cmp(&b.0));

        let tail = match &rp.rest {
            None => RowTail::Closed,
            Some(_) => RowTail::Open(self.fresh_var()),
        };
        let row_ty = Ty::Record(Row {
            fields: field_tys.clone(),
            tail: tail.clone(),
        });
        self.unify(expected, row_ty)?;

        let mut bindings = Vec::new();

        for fp in &rp.fields {
            let field_ty = field_tys
                .iter()
                .find(|(k, _)| k == &fp.name)
                .map(|(_, t)| self.subst.apply(t))
                .unwrap_or_else(|| self.fresh_ty());

            if let Some(sub_pat) = &fp.pattern {
                bindings.extend(self.infer_pattern(sub_pat, field_ty)?);
            } else {
                // Shorthand `{ age }` - the field name is the binding.
                self.node_types.insert(fp.node_id, field_ty.clone());
                bindings.push((fp.name.clone(), field_ty));
            }
        }

        if let Some(Some(rest_name)) = &rp.rest {
            if let RowTail::Open(v) = tail {
                bindings.push((
                    rest_name.clone(),
                    Ty::Record(Row {
                        fields: vec![],
                        tail: RowTail::Open(v),
                    }),
                ));
            }
        }

        Ok(bindings)
    }

    fn check_list_pattern(
        &mut self,
        lp: &ListPattern,
        expected: Ty,
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
            row.fields
                .into_iter()
                .find(|(k, _)| k == field)
                .map(|(_, t)| t)
        } else {
            None
        }
    }

    /// Like [`apply_imports`] but never fails: each import is attempted
    /// independently and errors are collected rather than returned immediately.
    /// On a failed import the binding name(s) are added to `env` with fresh
    /// type variables so downstream bindings don't cascade into spurious
    /// "unbound variable" errors.
    fn apply_imports_partial(
        &mut self,
        uses: &[UseDecl],
        base: Option<&Path>,
        loader: &mut crate::loader::Loader,
        env: &mut TypeEnv,
    ) -> Vec<TypeErrorAt> {
        let base = match base {
            Some(b) => b,
            None => return vec![],
        };
        let mut errors = Vec::new();
        for u in uses {
            let scheme = match loader.load(&u.path, base) {
                Ok(s) => s,
                Err(e) => {
                    errors.push(e);
                    // Give the binding(s) a fresh type variable so code that
                    // references this import doesn't produce cascading errors.
                    match &u.binding {
                        UseBinding::Ident(name, _, node_id) => {
                            let ty = self.fresh_ty();
                            self.node_types.insert(*node_id, ty.clone());
                            env.insert(name.clone(), Scheme::mono(ty));
                        }
                        UseBinding::Record(rp) => {
                            for f in &rp.fields {
                                let ty = self.fresh_ty();
                                self.node_types.insert(f.node_id, ty.clone());
                                env.insert(f.name.clone(), Scheme::mono(ty));
                            }
                        }
                    }
                    continue;
                }
            };
            match &u.binding {
                UseBinding::Ident(name, _, node_id) => {
                    let ty = self.instantiate(&scheme);
                    self.node_types.insert(*node_id, ty);
                    env.insert(name.clone(), scheme);
                }
                UseBinding::Record(rp) => {
                    let module_ty = self.instantiate(&scheme);
                    let module_ty = self.subst.apply(&module_ty);
                    for f in &rp.fields {
                        match self.extract_field_ty(&module_ty, &f.name) {
                            Some(field_ty) => {
                                let s = self.generalise(env, &field_ty);
                                self.node_types.insert(f.node_id, field_ty);
                                env.insert(f.name.clone(), s);
                            }
                            None => {
                                errors.push(TypeErrorAt::new(
                                    TypeError::FieldNotFound {
                                        field: f.name.clone(),
                                        record_ty: module_ty.clone(),
                                    },
                                    crate::error::Span::default(),
                                ));
                                let ty = self.fresh_ty();
                                self.node_types.insert(f.node_id, ty.clone());
                                env.insert(f.name.clone(), Scheme::mono(ty));
                            }
                        }
                    }
                }
            }
        }
        errors
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
                UseBinding::Ident(name, _, node_id) => {
                    // Import the whole module as a record value.
                    // Store the original scheme so each use site gets a fresh
                    // instantiation; storing Scheme::mono would share unification
                    // variables across call sites, breaking e.g. two uses of a
                    // polymorphic field like `maybe.withDefault`.
                    let ty = self.instantiate(&scheme);
                    self.node_types.insert(*node_id, ty);
                    env.insert(name.clone(), scheme);
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
                        self.node_types.insert(f.node_id, field_ty);
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
            env = match item {
                TopItem::Binding(b) => self.check_binding(b, env)?,
                TopItem::BindingGroup(bs) => self.check_binding_group(bs, env)?,
                TopItem::TypeDef(_) => env,
            };
        }

        let export_ty = self.infer(&env, &program.exports)?;
        Ok(self.subst.apply(&export_ty))
    }

    fn check_binding(&mut self, binding: &Binding, env: TypeEnv) -> Result<TypeEnv, TypeErrorAt> {
        let value_span = &binding.value.span;

        // For simple name bindings, add a fresh monomorphic placeholder *before*
        // checking the body to support self-recursion.
        let (rec_env, placeholder) = match &binding.pattern {
            Pattern::Ident(name, _, _) => {
                let ph = self.fresh_ty();
                let ext = env.extend_one(name.clone(), ph.clone());
                (ext, Some(ph))
            }
            _ => (env.clone(), None),
        };

        let scheme = if let Some(ann) = &binding.ty {
            let mut param_vars: HashMap<String, TyVar> = HashMap::new();
            let ann_ty = self
                .lower_ty(ann, &mut param_vars)
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

    /// Type-check a mutually recursive binding group (`let f = … and let g = …`).
    ///
    /// All binding names are added as fresh type variables before any body is
    /// checked so each body can refer to all names in the group.  After all
    /// bodies are checked the types are generalised together.
    fn check_binding_group(
        &mut self,
        bindings: &[Binding],
        env: TypeEnv,
    ) -> Result<TypeEnv, TypeErrorAt> {
        // Phase 1 - add a fresh placeholder for every name in the group.
        let mut rec_env = env.clone();
        let mut placeholders: Vec<(Pattern, Span, Option<Ty>)> = Vec::new();
        for binding in bindings {
            let ph = match &binding.pattern {
                Pattern::Ident(name, _, _) => {
                    let t = self.fresh_ty();
                    rec_env = rec_env.extend_one(name.clone(), t.clone());
                    Some(t)
                }
                _ => None,
            };
            placeholders.push((binding.pattern.clone(), binding.value.span.clone(), ph));
        }

        // Phase 2 - infer / check each body inside the extended env.
        let mut schemes: Vec<Scheme> = Vec::new();
        for (binding, (_, value_span, ph)) in bindings.iter().zip(placeholders.iter()) {
            let scheme = if let Some(ann) = &binding.ty {
                let mut param_vars: HashMap<String, TyVar> = HashMap::new();
                let ann_ty = self
                    .lower_ty(ann, &mut param_vars)
                    .map_err(|e| TypeErrorAt::new(e, value_span.clone()))?;
                if let Some(ph) = ph {
                    self.unify_at(ph.clone(), ann_ty.clone(), value_span)?;
                }
                self.check(&rec_env, &binding.value, ann_ty.clone())?;
                self.generalise(&env, &ann_ty)
            } else {
                let inferred = self.infer(&rec_env, &binding.value)?;
                let inferred = self.subst.apply(&inferred);
                if let Some(ph) = ph {
                    let ph = self.subst.apply(ph);
                    self.unify_at(ph, inferred.clone(), value_span)?;
                }
                self.generalise(&env, &inferred)
            };
            schemes.push(scheme);
        }

        // Phase 3 - bind all patterns in the output env.
        let mut new_env = env;
        for (binding, scheme) in bindings.iter().zip(schemes) {
            self.bind_pattern_scheme(&binding.pattern, scheme, &mut new_env)
                .map_err(|e| TypeErrorAt::new(e, binding.value.span.clone()))?;
        }
        Ok(new_env)
    }

    fn bind_pattern_scheme(
        &mut self,
        pat: &Pattern,
        scheme: Scheme,
        env: &mut TypeEnv,
    ) -> Result<(), TypeError> {
        match pat {
            Pattern::Ident(name, _, _) => {
                env.insert(name.clone(), scheme);
            }
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
    let mut subst = Subst::new();
    let (env, mut var_env) = builtin_env(&mut subst);
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::with_subst(var_env, subst);
    let mut loader = crate::loader::Loader::new();
    checker.check_program(program, env, path, &mut loader)
}

/// A named type binding for CLI display.
pub struct BindingInfo {
    pub name: String,
    pub scheme: Scheme,
}

// ── Shared helpers for elaborate* functions ───────────────────────────────────

/// After successfully checking a binding, record the node type for a simple
/// `let name = …` pattern so hover works on the binding site.
fn record_ident_node_type(binding: &Binding, checker: &mut Checker, env: &TypeEnv) {
    if let Pattern::Ident(name, _, node_id) = &binding.pattern {
        if let Some(scheme) = env.lookup(name) {
            let ty = checker.subst.apply(&scheme.ty);
            checker.node_types.insert(*node_id, ty);
        }
    }
}

/// On a failed binding, insert fresh type variables so downstream code doesn't
/// cascade into spurious "unbound variable" errors.
fn fresh_fallback_binding(checker: &mut Checker, env: &mut TypeEnv, binding: &Binding) {
    match &binding.pattern {
        Pattern::Ident(name, _, node_id) => {
            let ty = checker.fresh_ty();
            checker.node_types.insert(*node_id, ty.clone());
            env.insert(name.clone(), Scheme::mono(ty));
        }
        Pattern::Record(rp) => {
            for fp in &rp.fields {
                let bind_name = fp
                    .pattern
                    .as_ref()
                    .and_then(|p| {
                        if let Pattern::Ident(n, _, _) = p {
                            Some(n.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| fp.name.clone());
                let ty = checker.fresh_ty();
                env.insert(bind_name, Scheme::mono(ty));
            }
        }
        _ => {}
    }
}

/// Type-check a program and return top-level binding info for CLI display,
/// plus the export type.
pub fn elaborate_bindings(
    program: &Program,
    path: Option<&Path>,
) -> Result<(Vec<BindingInfo>, Ty), TypeErrorAt> {
    let mut subst = Subst::new();
    let (env, mut var_env) = builtin_env(&mut subst);
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::with_subst(var_env, subst);
    let mut loader = crate::loader::Loader::new();
    let mut env = env;

    checker.apply_imports(&program.uses, path, &mut loader, &mut env)?;

    let mut bindings = Vec::new();
    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                env = checker.check_binding(b, env)?;
                collect_binding_info(&b.pattern, &env, &checker, &mut bindings);
            }
            TopItem::BindingGroup(bs) => {
                env = checker.check_binding_group(bs, env)?;
                for b in bs {
                    collect_binding_info(&b.pattern, &env, &checker, &mut bindings);
                }
            }
            TopItem::TypeDef(_) => {}
        }
    }

    let export_ty = checker.infer(&env, &program.exports)?;
    let export_ty = checker.subst.apply(&export_ty);

    // Apply final substitution to binding schemes.
    for b in &mut bindings {
        b.scheme = checker.subst.apply_scheme(&b.scheme);
    }

    Ok((bindings, export_ty))
}

fn collect_binding_info(
    pat: &Pattern,
    env: &TypeEnv,
    checker: &Checker,
    out: &mut Vec<BindingInfo>,
) {
    match pat {
        Pattern::Ident(name, _, _) => {
            if let Some(scheme) = env.lookup(name) {
                let scheme = checker.subst.apply_scheme(scheme);
                out.push(BindingInfo {
                    name: name.clone(),
                    scheme,
                });
            }
        }
        Pattern::Record(rp) => {
            for fp in &rp.fields {
                let name = fp
                    .pattern
                    .as_ref()
                    .and_then(|p| {
                        if let Pattern::Ident(n, _, _) = p {
                            Some(n)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(&fp.name);
                if let Some(scheme) = env.lookup(name) {
                    let scheme = checker.subst.apply_scheme(scheme);
                    out.push(BindingInfo {
                        name: name.clone(),
                        scheme,
                    });
                }
            }
        }
        _ => {}
    }
}

/// Type-check a program.
///
/// Returns:
/// - `node_types`: a map from every expression's `NodeId` to its fully-resolved type.
/// - The type of the module export expression.
///
/// `path` is the file being checked; it is used to resolve relative `use`
/// paths.  Pass `None` when checking an in-memory buffer (imports are skipped).
pub fn elaborate(
    program: &Program,
    path: Option<&Path>,
) -> Result<(HashMap<NodeId, Ty>, Ty), TypeErrorAt> {
    let mut subst = Subst::new();
    let (env, mut var_env) = builtin_env(&mut subst);
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::with_subst(var_env, subst);
    let mut loader = crate::loader::Loader::new();
    let mut env = env;

    checker.apply_imports(&program.uses, path, &mut loader, &mut env)?;

    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                env = checker.check_binding(b, env)?;
                record_ident_node_type(b, &mut checker, &env);
            }
            TopItem::BindingGroup(bs) => {
                env = checker.check_binding_group(bs, env)?;
                for b in bs {
                    record_ident_node_type(b, &mut checker, &env);
                }
            }
            TopItem::TypeDef(_) => {}
        }
    }

    let export_ty = checker.infer(&env, &program.exports)?;
    let export_ty = checker.subst.apply(&export_ty);

    // Apply the final substitution to every recorded type so all type variables
    // are resolved to their concrete types.
    let node_types: HashMap<NodeId, Ty> = checker
        .node_types
        .into_iter()
        .map(|(id, ty)| (id, checker.subst.apply(&ty)))
        .collect();

    Ok((node_types, export_ty))
}

/// Like [`elaborate`] but also returns the final top-level [`TypeEnv`] so
/// callers (e.g. the LSP) can enumerate all names in scope for completions.
pub fn elaborate_with_env(
    program: &Program,
    path: Option<&Path>,
) -> Result<(HashMap<NodeId, Ty>, TypeEnv, Ty), TypeErrorAt> {
    let mut subst = Subst::new();
    let (base_env, mut var_env) = builtin_env(&mut subst);
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::with_subst(var_env, subst);
    let mut loader = crate::loader::Loader::new();
    let mut env = base_env;

    checker.apply_imports(&program.uses, path, &mut loader, &mut env)?;

    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                env = checker.check_binding(b, env)?;
                record_ident_node_type(b, &mut checker, &env);
            }
            TopItem::BindingGroup(bs) => {
                env = checker.check_binding_group(bs, env)?;
                for b in bs {
                    record_ident_node_type(b, &mut checker, &env);
                }
            }
            TopItem::TypeDef(_) => {}
        }
    }

    let export_ty = checker.infer(&env, &program.exports)?;
    let export_ty = checker.subst.apply(&export_ty);

    // Apply final substitution to env and node_types.
    let resolved_env: TypeEnv = {
        let mut e = TypeEnv::new();
        for (name, scheme) in env.iter() {
            e.insert(name.clone(), checker.subst.apply_scheme(scheme));
        }
        e
    };
    let node_types: HashMap<NodeId, Ty> = checker
        .node_types
        .into_iter()
        .map(|(id, ty)| (id, checker.subst.apply(&ty)))
        .collect();

    Ok((node_types, resolved_env, export_ty))
}

/// Like [`elaborate_with_env`] but never fails - type errors are collected and
/// returned alongside whatever partial information was gathered.  Bindings that
/// produce errors are given fresh type variables so subsequent bindings can
/// still be checked.
///
/// This is the function the LSP should use so that hover / completions remain
/// available even when the file contains type errors.
pub fn elaborate_with_env_partial(
    program: &Program,
    path: Option<&Path>,
) -> (HashMap<NodeId, Ty>, TypeEnv, Vec<TypeErrorAt>) {
    let mut subst = Subst::new();
    let (base_env, mut var_env) = builtin_env(&mut subst);
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::with_subst(var_env, subst);
    let mut loader = crate::loader::Loader::new();
    let mut env = base_env;

    let mut errors = checker.apply_imports_partial(&program.uses, path, &mut loader, &mut env);

    for item in &program.items {
        match item {
            TopItem::Binding(binding) => match checker.check_binding(binding, env.clone()) {
                Ok(new_env) => {
                    env = new_env;
                    record_ident_node_type(binding, &mut checker, &env);
                }
                Err(e) => {
                    errors.push(e);
                    fresh_fallback_binding(&mut checker, &mut env, binding);
                }
            },
            TopItem::BindingGroup(bs) => match checker.check_binding_group(bs, env.clone()) {
                Ok(new_env) => {
                    env = new_env;
                    for b in bs {
                        record_ident_node_type(b, &mut checker, &env);
                    }
                }
                Err(e) => {
                    errors.push(e);
                    for b in bs {
                        fresh_fallback_binding(&mut checker, &mut env, b);
                    }
                }
            },
            TopItem::TypeDef(_) => {}
        }
    }

    // Best-effort export type inference - ignore errors.
    let _ = checker.infer(&env, &program.exports);

    // Apply final substitution.
    let resolved_env: TypeEnv = {
        let mut e = TypeEnv::new();
        for (name, scheme) in env.iter() {
            e.insert(name.clone(), checker.subst.apply_scheme(scheme));
        }
        e
    };
    let node_types: HashMap<NodeId, Ty> = checker
        .node_types
        .into_iter()
        .map(|(id, ty)| (id, checker.subst.apply(&ty)))
        .collect();

    (node_types, resolved_env, errors)
}
