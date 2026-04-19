use crate::ast::{
    self, BinOp, Binding, Expr, ExprKind, FieldType, ImplDef, ListPattern, Literal, MatchArm,
    NodeId, Pattern, Program, RecordField, RecordPattern, RecordType, TopItem, TraitDef, Type,
    UnOp, UseBinding, UseDecl,
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
    /// The type this variant wraps (None for unit variants).
    /// Using AST types here so we can lower them fresh per instantiation.
    pub wraps: Option<ast::Type>,
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
    /// Return all variant names that belong to the given type name.
    pub fn variants_of_type(&self, type_name: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(_, info)| info.type_name == type_name)
            .map(|(name, _)| name.clone())
            .collect()
    }
    /// Iterate over all entries.
    pub fn all(&self) -> impl Iterator<Item = (&String, &VariantInfo)> {
        self.0.iter()
    }
}

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
                env.insert(
                    variant.name.clone(),
                    VariantInfo {
                        type_name: td.name.clone(),
                        type_params: td.params.clone(),
                        wraps: variant.wraps.clone(),
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
        constraints: vec![],
        ty,
    };

    crate::builtin::populate_env(s, &mut env);

    let v0 = s.fresh_var();
    let v1 = s.fresh_var();

    // Typed show primitives (building blocks for ToText trait impls)
    env.insert(
        "showNum".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Num), Box::new(Ty::Text))),
    );
    env.insert(
        "showBool".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Bool), Box::new(Ty::Text))),
    );
    env.insert(
        "showText".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Text), Box::new(Ty::Text))),
    );
    // showRecord : { .. } -> Text  (open row → accepts any record)
    let rv = s.fresh_var();
    env.insert(
        "showRecord".into(),
        Scheme {
            vars: vec![],
            row_vars: vec![rv],
            constraints: vec![],
            ty: Ty::Func(
                Box::new(Ty::Record(Row {
                    fields: vec![],
                    tail: RowTail::Open(rv),
                })),
                Box::new(Ty::Text),
            ),
        },
    );

    // Basic functions
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
            Box::new(Ty::mk_con("Maybe", &[Ty::Num])),
        )),
    );

    // range : Num -> Num -> List Num
    env.insert(
        "range".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::Num),
            Box::new(Ty::Func(
                Box::new(Ty::Num),
                Box::new(Ty::mk_con("List", &[Ty::Num])),
            )),
        )),
    );

    // List functions - use vars v0, v1
    let list_a = Ty::mk_con("List", &[Ty::Var(v0)]);
    let list_b = Ty::mk_con("List", &[Ty::Var(v1)]);

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
            Box::new(Ty::mk_con("List", &[Ty::Num])),
            Box::new(Ty::mk_con("List", &[Ty::Num])),
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
    let list_num = Ty::mk_con("List", &[Ty::Num]);
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
    let result_text = Ty::mk_con("Result", &[Ty::Text, Ty::Text]);
    env.insert(
        "readFile".into(),
        Scheme::mono(Ty::Func(Box::new(Ty::Text), Box::new(result_text.clone()))),
    );

    // writeFile : Text -> Text -> Result {} Text
    let result_unit = Ty::mk_con(
        "Result",
        &[
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
                Box::new(Ty::mk_con("List", &[Ty::Text])),
            )),
        )),
    );
    // join : Text -> List Text -> Text
    env.insert(
        "join".into(),
        Scheme::mono(Ty::Func(
            Box::new(Ty::Text),
            Box::new(Ty::Func(
                Box::new(Ty::mk_con("List", &[Ty::Text])),
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
                Box::new(Ty::mk_con("Result", &[Ty::Var(v0), Ty::Var(v1)])),
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
                    Box::new(Ty::mk_con("Maybe", &[Ty::Var(v0)])),
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
                    Box::new(Ty::mk_con("Result", &[Ty::Var(0), Ty::Var(1)])),
                    Box::new(Ty::mk_con("Result", &[Ty::Var(0), Ty::Var(2)])),
                )),
            ),
        ),
    );

    // Maybe variants: Some a, None
    var_env.insert(
        "Some".into(),
        VariantInfo {
            type_name: "Maybe".into(),
            type_params: vec!["a".into()],
            wraps: Some(ast::Type::Var("a".into())),
        },
    );
    var_env.insert(
        "None".into(),
        VariantInfo {
            type_name: "Maybe".into(),
            type_params: vec!["a".into()],
            wraps: None,
        },
    );

    // Result variants: Ok a, Err b
    var_env.insert(
        "Ok".into(),
        VariantInfo {
            type_name: "Result".into(),
            type_params: vec!["a".into(), "b".into()],
            wraps: Some(ast::Type::Var("a".into())),
        },
    );
    var_env.insert(
        "Err".into(),
        VariantInfo {
            type_name: "Result".into(),
            type_params: vec!["a".into(), "b".into()],
            wraps: Some(ast::Type::Var("b".into())),
        },
    );

    (env, var_env)
}

/// Return type of `instantiate_variant`: (result_type, optional wraps type).
type VariantInstance = (Ty, Option<Ty>);

// ── The typechecker ───────────────────────────────────────────────────────────

// ── Arity environment ─────────────────────────────────────────────────────────

/// Maps type constructor names to their expected number of type parameters.
/// Used during AST→Ty lowering to catch arity mismatches early (e.g. `Num a`).
pub type ArityEnv = HashMap<String, usize>;

/// Pre-populate built-in arities and scan user TypeDefs to build the ArityEnv.
pub fn build_arity_env(items: &[TopItem]) -> ArityEnv {
    let mut env: ArityEnv = HashMap::new();
    // Built-in types
    env.insert("Num".into(), 0);
    env.insert("Text".into(), 0);
    env.insert("Bool".into(), 0);
    env.insert("List".into(), 1);
    env.insert("Maybe".into(), 1);
    env.insert("Result".into(), 2);
    // Scan user-defined ADTs
    for item in items {
        if let TopItem::TypeDef(td) = item {
            env.insert(td.name.clone(), td.params.len());
        }
    }
    env
}

// ── Checker ──────────────────────────────────────────────────────────────────

// Checker owns the mutable state for one type-checking session (one module).
//
// `subst`       - the live substitution; grows as constraints are solved.
// `variant_env` - constructor metadata; read-only after construction.
// `trait_env`   - declared traits; used for TraitCall typing and impl completeness.
// `node_types`  - side channel: every AST node's inferred type, keyed by
//                 NodeId.  Used by the LSP for hover/completion information.
//                 Types here still contain unification variables; callers
//                 apply the final `subst` to obtain ground types.
#[allow(clippy::type_complexity)]
pub struct Checker {
    pub subst: Subst,
    pub variant_env: VariantEnv,
    pub arity_env: ArityEnv,
    pub trait_env: HashMap<String, TraitDef>,
    /// Maps each expression's NodeId to its inferred type (with type vars, resolved at end).
    pub node_types: HashMap<NodeId, Ty>,
    /// Accumulated trait constraints: `(trait_name, fresh_var)`.
    pub constraint_map: Vec<(String, TyVar)>,
    /// Typed holes: `(NodeId, Span)` of each `_` expression encountered.
    /// The resolved type is read from `node_types` after substitution.
    pub holes: Vec<(NodeId, Span)>,
    /// Available trait impls: `(trait_name, type_canonical_name) -> defining_module`.
    pub impl_env: HashMap<(String, String), String>,
    /// Parameterized trait impls: `(trait_name, target_type_pattern, constraints)`.
    pub param_impl_env: Vec<(String, Type, Vec<(String, String)>)>,
    /// Trait calls that need impl validation: `(trait_name, fresh_var, span)`.
    /// Only populated by TraitCall inference (not by instantiate).
    trait_call_constraints: Vec<(String, TyVar, Span)>,
    /// Ident nodes resolved to a trait method via unambiguous lookup.
    /// Maps `NodeId` → `(trait_name, method_name)` so the lowerer can emit
    /// the correct dict field access instead of a bare variable reference.
    pub resolved_trait_methods: HashMap<NodeId, (String, String)>,
}

impl Checker {
    pub fn new(variant_env: VariantEnv) -> Self {
        Checker {
            subst: Subst::new(),
            variant_env,
            arity_env: ArityEnv::new(),
            trait_env: HashMap::new(),
            node_types: HashMap::new(),
            constraint_map: Vec::new(),
            holes: Vec::new(),
            impl_env: HashMap::new(),
            param_impl_env: Vec::new(),
            trait_call_constraints: Vec::new(),
            resolved_trait_methods: HashMap::new(),
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
            arity_env: ArityEnv::new(),
            trait_env: HashMap::new(),
            node_types: HashMap::new(),
            constraint_map: Vec::new(),
            holes: Vec::new(),
            impl_env: HashMap::new(),
            param_impl_env: Vec::new(),
            trait_call_constraints: Vec::new(),
            resolved_trait_methods: HashMap::new(),
        }
    }

    /// Register an impl in either `impl_env` (concrete) or `param_impl_env`
    /// (parameterized, when constraints are present).
    /// Returns an error if a duplicate concrete impl is detected.
    fn register_impl(&mut self, id: &ImplDef) -> Result<(), TypeErrorAt> {
        // Reject impls for open record types (rows with `..`).
        if let ast::Type::Record(ref rt) = id.target_type {
            if rt.open {
                return Err(TypeErrorAt::new(
                    TypeError::ImplForOpenRecord {
                        trait_name: id.trait_name.clone(),
                    },
                    id.type_name_span.clone(),
                ));
            }
        }

        if id.impl_constraints.is_empty() {
            let key = (id.trait_name.clone(), id.type_name.clone());
            if let Some(existing) = self.impl_env.get(&key) {
                if existing != "<local>" {
                    // Local module redefines an impl from an imported module
                    return Err(TypeErrorAt::new(
                        TypeError::DuplicateImpl {
                            trait_name: id.trait_name.clone(),
                            type_name: id.type_name.clone(),
                        },
                        id.trait_name_span.clone(),
                    ));
                }
            }
            self.impl_env.insert(key, "<local>".to_string());
        } else {
            self.param_impl_env.push((
                id.trait_name.clone(),
                id.target_type.clone(),
                id.impl_constraints.clone(),
            ));
        }
        Ok(())
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
        // Keep a snapshot of the var renaming for constraint propagation below.
        let local_tys_snapshot = local_tys.clone();
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
        let result = tmp.apply(&scheme.ty);

        // Propagate constraints from the scheme into the current context,
        // renaming the quantified vars to their fresh counterparts.
        for (trait_name, scheme_var) in &scheme.constraints {
            if let Some(Ty::Var(fresh)) = local_tys_snapshot.get(scheme_var) {
                self.constraint_map.push((trait_name.clone(), *fresh));
            }
        }

        result
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
        let mut vars: Vec<TyVar> = ty_tvs.difference(&env_tvs).copied().collect();
        vars.sort();
        let mut row_vars: Vec<TyVar> = ty_rvs.difference(&env_rvs).copied().collect();
        row_vars.sort();
        Scheme {
            vars,
            row_vars,
            constraints: {
                let generalised: HashSet<TyVar> = ty_tvs.difference(&env_tvs).copied().collect();
                let mut seen = std::collections::HashSet::new();
                self.constraint_map
                    .iter()
                    .filter_map(|(trait_name, fresh_var)| {
                        match self.subst.apply(&Ty::Var(*fresh_var)) {
                            Ty::Var(v) if generalised.contains(&v) => {
                                let pair = (trait_name.clone(), v);
                                if seen.insert(pair.clone()) {
                                    Some(pair)
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        }
                    })
                    .collect()
            },
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
            Type::Constructor(name) => match name.as_str() {
                "Num" => Ok(Ty::Num),
                "Text" => Ok(Ty::Text),
                "Bool" => Ok(Ty::Bool),
                _ => Ok(Ty::Con(name.clone())),
            },
            Type::App { .. } => {
                // Flatten the left-associative App tree to find the base and
                // all arguments. E.g. `App(App(Con("Result"), Con("Num")), Con("Text"))`
                // flattens to base=Constructor("Result"), args=[Constructor("Num"), Constructor("Text")].
                let (base, args) = flatten_ast_app(ty);
                match base {
                    // Known constructor: check arity, then lower.
                    Type::Constructor(name) => {
                        // If this constructor is in the ArityEnv, verify its arity.
                        if let Some(&expected) = self.arity_env.get(name.as_str()) {
                            if args.len() != expected {
                                return Err(TypeError::ArityMismatch {
                                    name: name.clone(),
                                    expected,
                                    actual: args.len(),
                                });
                            }
                        }
                        // Lower the constructor and each argument, building a curried App chain.
                        let base_ty = self.lower_ty(base, param_vars)?;
                        args.iter().try_fold(base_ty, |acc, arg| {
                            let arg_ty = self.lower_ty(arg, param_vars)?;
                            Ok(Ty::App(Box::new(acc), Box::new(arg_ty)))
                        })
                    }
                    // Variable base (HKT): `f a` — no arity check, just lower.
                    _ => {
                        let base_ty = self.lower_ty(base, param_vars)?;
                        args.iter().try_fold(base_ty, |acc, arg| {
                            let arg_ty = self.lower_ty(arg, param_vars)?;
                            Ok(Ty::App(Box::new(acc), Box::new(arg_ty)))
                        })
                    }
                }
            }
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
    // Returns `(result_ty, wraps_ty)` where:
    //   • `result_ty` is the ADT type with fresh vars for each param,
    //                  e.g. `Maybe ?a` with a fresh ?a per call.
    //   • `wraps_ty`  is the type the constructor wraps, or None for unit
    //                  constructors (e.g. `None`).
    //
    // The wrapped AST type (stored in VariantInfo) is lowered through
    // `lower_ty` using the same `param_vars` map that was populated when
    // building `result_ty`.  This guarantees the param names unify correctly:
    // `Some a` gives a wrapped type `?a` where ?a is the same variable as
    // in `Maybe ?a`.
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

        let result_ty = Ty::mk_con(&info.type_name, &fresh_args);

        let wraps_ty = if let Some(wt) = &info.wraps {
            Some(self.lower_ty(wt, &mut param_vars)?)
        } else {
            None
        };

        Ok((result_ty, wraps_ty))
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

            // Typed hole: allocate a fresh type variable and record this hole.
            // The resolved type will be reported as a diagnostic after inference.
            ExprKind::Hole => {
                let ty = self.fresh_ty();
                self.holes.push((expr.id, span.clone()));
                Ok(ty)
            }

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
                Ok(Ty::mk_con("List", &[self.subst.apply(&elem)]))
            }

            // Variable reference: look up the scheme and instantiate it.
            // Instantiation replaces quantified vars with fresh ones so that
            // this particular use of the variable gets its own type variables.
            ExprKind::Ident(name) => match env.lookup(name) {
                Some(scheme) => Ok(self.instantiate(scheme)),
                None => {
                    // Unambiguous trait method shorthand: if exactly one trait
                    // defines a method with this name, resolve it automatically.
                    let mut matches: Vec<(&str, &ast::TraitMethod)> = self
                        .trait_env
                        .values()
                        .filter_map(|td| {
                            td.methods
                                .iter()
                                .find(|m| m.name == *name)
                                .map(|m| (td.name.as_str(), m))
                        })
                        .collect();
                    // Collect the trait names for the Checker lookup below (avoid
                    // borrow conflict when we later call self.lower_ty).
                    if matches.len() == 1 {
                        let (trait_name, method) = matches.remove(0);
                        let td = self.trait_env.get(trait_name).unwrap();
                        let trait_param = td.type_param.clone();
                        let trait_name = trait_name.to_string();
                        let method_ty = method.ty.clone();
                        let trait_param_var = self.subst.fresh_var();
                        let mut param_vars: HashMap<String, TyVar> =
                            HashMap::from([(trait_param, trait_param_var)]);
                        let ty = self
                            .lower_ty(&method_ty, &mut param_vars)
                            .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                        self.constraint_map.push((trait_name.clone(), trait_param_var));
                        self.trait_call_constraints.push((
                            trait_name.clone(),
                            trait_param_var,
                            span.clone(),
                        ));
                        self.resolved_trait_methods.insert(expr.id, (trait_name, name.clone()));
                        Ok(ty)
                    } else if matches.len() > 1 {
                        let mut options: Vec<String> = matches
                            .iter()
                            .map(|(tn, m)| format!("{}.{}", tn, m.name))
                            .collect();
                        options.sort();
                        Err(TypeErrorAt::new(
                            TypeError::AmbiguousTraitMethod {
                                name: name.clone(),
                                options,
                            },
                            span.clone(),
                        ))
                    } else {
                        Err(TypeErrorAt::new(
                            TypeError::UnboundVariable(name.clone()),
                            span.clone(),
                        ))
                    }
                }
            },

            // Variant used without a payload (e.g. bare `None` or `Some` as
            // a first-class function).
            // If the variant wraps a type, treat the bare name as a
            // curried constructor: `wraps_ty -> ConType`.
            ExprKind::Variant {
                name,
                payload: None,
            } => {
                let (result_ty, wraps_ty) = self
                    .instantiate_variant(name)
                    .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                if let Some(wt) = wraps_ty {
                    Ok(Ty::Func(Box::new(wt), Box::new(result_ty)))
                } else {
                    Ok(result_ty)
                }
            }

            ExprKind::Variant {
                name,
                payload: Some(payload_expr),
            } => {
                let (result_ty, wraps_ty) = self
                    .instantiate_variant(name)
                    .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                let wt = wraps_ty.ok_or_else(|| {
                    TypeErrorAt::new(
                        TypeError::UnboundVariant(format!("{} is a unit variant", name)),
                        span.clone(),
                    )
                })?;
                self.check(env, payload_expr, wt)?;
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

            ExprKind::MatchExpr { scrutinee, arms } => {
                self.infer_match_expr(env, scrutinee, arms, span)
            }

            ExprKind::TraitCall {
                trait_name,
                method_name,
            } => {
                // Look up the trait and find the method's declared type.
                // The type parameter is treated as a fresh unification variable.
                let trait_def = self.trait_env.get(trait_name).cloned();
                match trait_def {
                    Some(td) => {
                        match td.methods.iter().find(|m| m.name == *method_name) {
                            Some(method) => {
                                // Pre-seed param_vars with the trait's type parameter
                                // so `lower_ty` connects it to the same fresh var
                                // everywhere it appears in the method signature.
                                let trait_param_var = self.subst.fresh_var();
                                let mut param_vars: HashMap<String, TyVar> =
                                    HashMap::from([(td.type_param.clone(), trait_param_var)]);
                                let ty = self
                                    .lower_ty(&method.ty, &mut param_vars)
                                    .map_err(|e| TypeErrorAt::new(e, span.clone()))?;
                                // Record constraint on the trait's type param.
                                self.constraint_map
                                    .push((trait_name.clone(), trait_param_var));
                                self.trait_call_constraints.push((
                                    trait_name.clone(),
                                    trait_param_var,
                                    span.clone(),
                                ));
                                Ok(ty)
                            }
                            None => Err(TypeErrorAt::new(
                                TypeError::UnboundVariable(format!(
                                    "{}.{}",
                                    trait_name, method_name
                                )),
                                span.clone(),
                            )),
                        }
                    }
                    None => {
                        // Trait not yet known (e.g. cross-module); fall back to fresh var.
                        Ok(Ty::Var(self.subst.fresh_var()))
                    }
                }
            }

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
            _ if tl.con_name() == Some("List") => {
                let (_, args) = tl.flatten_app();
                let elem = (*args.first().expect("List must have exactly 1 type argument")).clone();
                let list_ty = Ty::mk_con("List", &[elem.clone()]);
                self.check(env, right, list_ty)?;
                Ok(Ty::mk_con("List", &[self.subst.apply(&elem)]))
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
        self.unify(tl, Ty::mk_con("Result", &[a.clone(), e.clone()]))
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
                Box::new(Ty::mk_con("Result", &[b, e.clone()])),
            ),
            &right.span,
        )?;
        let b = self.subst.apply(&b_c);
        let e = self.subst.apply(&e);
        Ok(Ty::mk_con("Result", &[b, e]))
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

    /// `match expr in | pat -> body ...`  — has an explicit scrutinee.
    /// Returns the body type directly (not a function type).
    fn infer_match_expr(
        &mut self,
        env: &TypeEnv,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: &Span,
    ) -> Result<Ty, TypeErrorAt> {
        let t_scrut = self.infer(env, scrutinee)?;
        let t_scrut = self.subst.apply(&t_scrut);
        let t_out = self.fresh_ty();

        for arm in arms {
            let t_in_c = self.subst.apply(&t_scrut);
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

        // Check exhaustiveness after inferring all arms.
        let t_scrut_resolved = self.subst.apply(&t_scrut);
        self.check_exhaustiveness(&t_scrut_resolved, arms, span)?;

        Ok(self.subst.apply(&t_out))
    }

    /// Check whether the match arms cover all constructors of a sum type.
    ///
    /// Only applies when the scrutinee type is a known `Con("TypeName", _)`.
    /// A wildcard (`_`) or a bare ident pattern counts as a catch-all.
    /// If no catch-all exists and some constructors are missing, report an error.
    fn check_exhaustiveness(
        &self,
        scrutinee_ty: &Ty,
        arms: &[MatchArm],
        span: &Span,
    ) -> Result<(), TypeErrorAt> {
        let type_name = match scrutinee_ty.con_name() {
            Some(name) => name,
            None => return Ok(()),
        };

        let all_variants = self.variant_env.variants_of_type(type_name);
        if all_variants.is_empty() {
            return Ok(());
        }

        // Check if any arm is a catch-all (wildcard or bare ident).
        let has_catch_all = arms
            .iter()
            .any(|arm| matches!(arm.pattern, Pattern::Wildcard | Pattern::Ident(..)));
        if has_catch_all {
            return Ok(());
        }

        // Collect variant names mentioned in pattern heads.
        let mut covered: HashSet<String> = HashSet::new();
        for arm in arms {
            collect_variant_names(&arm.pattern, &mut covered);
        }

        let missing: Vec<String> = all_variants
            .into_iter()
            .filter(|v| !covered.contains(v))
            .collect();

        if missing.is_empty() {
            Ok(())
        } else {
            Err(TypeErrorAt::new(
                TypeError::NonExhaustiveMatch(missing),
                span.clone(),
            ))
        }
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
        let (result_ty, wraps_ty) = self.instantiate_variant(name)?;
        self.unify(expected, result_ty)?;

        match (wraps_ty, payload) {
            (Some(wt), None) | (Some(wt), Some(Pattern::Wildcard)) => {
                let _ = wt;
                Ok(vec![])
            }
            (Some(wt), Some(p)) => self.infer_pattern(p, wt),
            (None, None) | (None, Some(Pattern::Wildcard)) => Ok(vec![]),
            (None, Some(p)) => self.infer_pattern(
                p,
                Ty::Record(Row {
                    fields: vec![],
                    tail: RowTail::Closed,
                }),
            ),
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
        self.unify(expected, Ty::mk_con("List", &[elem.clone()]))?;
        let elem = self.subst.apply(&elem);

        let mut bindings = Vec::new();
        for p in &lp.elements {
            let elem_c = self.subst.apply(&elem);
            bindings.extend(self.infer_pattern(p, elem_c)?);
        }
        if let Some(Some(rest)) = &lp.rest {
            bindings.push((rest.clone(), Ty::mk_con("List", &[self.subst.apply(&elem)])));
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
            let exports = match loader.load(&u.path, base) {
                Ok(e) => e,
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
            let scheme = exports.scheme;
            self.variant_env.merge(exports.variant_env);
            self.trait_env.extend(exports.trait_env);
            for (key, source) in exports.impl_env {
                self.impl_env.entry(key).or_insert(source);
            }
            self.param_impl_env.extend(exports.param_impl_env);
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
            let exports = loader.load(&u.path, base)?;
            let scheme = exports.scheme;
            self.variant_env.merge(exports.variant_env);
            self.trait_env.extend(exports.trait_env);
            // Merge impl_env with duplicate detection: allow diamond imports
            // (same source module) but reject independent duplicates.
            for (key, source) in exports.impl_env {
                if let Some(existing) = self.impl_env.get(&key) {
                    if existing != &source {
                        return Err(TypeErrorAt::new(
                            TypeError::DuplicateImpl {
                                trait_name: key.0,
                                type_name: key.1,
                            },
                            crate::error::Span::default(),
                        ));
                    }
                } else {
                    self.impl_env.insert(key, source);
                }
            }
            self.param_impl_env.extend(exports.param_impl_env);

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

        // Validate trait definitions up front (method must reference type param),
        // but do NOT register them yet — traits are registered sequentially so
        // that `use … in` must appear after the trait definition.
        for item in &program.items {
            if let TopItem::TraitDef(td) = item {
                for method in &td.methods {
                    if !ast_type_contains_var(&method.ty, &td.type_param) {
                        return Err(TypeErrorAt::new(
                            TypeError::TraitMethodMissingParam {
                                trait_name: td.name.clone(),
                                method_name: method.name.clone(),
                                type_param: td.type_param.clone(),
                            },
                            method.name_span.clone(),
                        ));
                    }
                }
            }
        }

        for item in &program.items {
            env = match item {
                TopItem::Binding(b) => self.check_binding(b, env)?,
                TopItem::BindingGroup(bs) => self.check_binding_group(bs, env)?,
                TopItem::ImplDef(id) => {
                    self.check_impl_completeness(id)?;
                    self.check_impl_annotations(id)?;
                    self.register_impl(id)?;
                    let td = self.trait_env.get(&id.trait_name).cloned();
                    let b = synth_impl_binding(id, td.as_ref());
                    let new_env = self.check_binding(&b, env)?;
                    backfill_impl_method_types(id, &new_env, &mut self.node_types, &self.subst);
                    new_env
                }
                TopItem::TypeDef(_) => env,
                TopItem::TraitDef(td) => {
                    self.trait_env.insert(td.name.clone(), td.clone());
                    env
                }
            };
        }

        self.check_trait_constraints()?;

        let export_ty = self.infer(&env, &program.exports)?;
        Ok(self.subst.apply(&export_ty))
    }

    /// Verify that an impl block provides every method declared in its trait.
    fn check_impl_completeness(&self, id: &ImplDef) -> Result<(), TypeErrorAt> {
        let trait_def = match self.trait_env.get(&id.trait_name) {
            Some(td) => td,
            None => {
                return Err(TypeErrorAt::new(
                    TypeError::UndeclaredTrait {
                        trait_name: id.trait_name.clone(),
                    },
                    id.trait_name_span.clone(),
                ));
            }
        };
        let mut missing = Vec::new();
        for method in &trait_def.methods {
            let provided = id.methods.iter().any(
                |m| matches!(&m.pattern, crate::ast::Pattern::Ident(n, _, _) if n == &method.name),
            );
            if !provided {
                missing.push(method.name.clone());
            }
        }
        if !missing.is_empty() {
            return Err(TypeErrorAt::new(
                TypeError::IncompleteImpl {
                    trait_name: id.trait_name.clone(),
                    type_name: id.type_name.clone(),
                    missing,
                },
                id.trait_name_span.clone(),
            ));
        }
        // Check for extra methods not declared in the trait
        let mut extra = Vec::new();
        for m in &id.methods {
            let name = match &m.pattern {
                crate::ast::Pattern::Ident(n, _, _) => n.clone(),
                _ => continue,
            };
            if !trait_def.methods.iter().any(|tm| tm.name == name) {
                extra.push(name);
            }
        }
        if !extra.is_empty() {
            return Err(TypeErrorAt::new(
                TypeError::ExtraImplMethods {
                    trait_name: id.trait_name.clone(),
                    type_name: id.type_name.clone(),
                    extra,
                },
                id.trait_name_span.clone(),
            ));
        }
        Ok(())
    }

    /// If an impl method carries a user type annotation, verify it is consistent
    /// with the type derived from the trait definition (after substituting the
    /// trait's type parameter for the impl target type).
    fn check_impl_annotations(&mut self, id: &ImplDef) -> Result<(), TypeErrorAt> {
        let td = match self.trait_env.get(&id.trait_name) {
            Some(td) => td.clone(),
            None => return Ok(()),
        };
        for impl_method in &id.methods {
            let user_ann = match &impl_method.ty {
                Some(ty) => ty,
                None => continue,
            };
            let method_name = match &impl_method.pattern {
                Pattern::Ident(n, _, _) => n.clone(),
                _ => continue,
            };
            let trait_method = match td.methods.iter().find(|m| m.name == method_name) {
                Some(m) => m,
                None => continue,
            };
            let derived_ty = subst_type_var(&trait_method.ty, &td.type_param, &id.target_type);
            // Lower both to internal types and unify to check consistency.
            let mut pvars = HashMap::new();
            let derived = self
                .lower_ty(&derived_ty, &mut pvars)
                .map_err(|e| TypeErrorAt::new(e, impl_method.value.span.clone()))?;
            let user = self
                .lower_ty(user_ann, &mut pvars)
                .map_err(|e| TypeErrorAt::new(e, impl_method.value.span.clone()))?;
            self.unify_at(derived, user, &impl_method.value.span)?;
        }
        Ok(())
    }

    /// After all items are processed, verify that every concrete trait call
    /// has a matching impl in scope.
    fn check_trait_constraints(&self) -> Result<(), TypeErrorAt> {
        // Check both direct TraitCall constraints (with spans) and propagated
        // instantiation constraints from constraint_map.
        let mut checked: HashSet<(String, String)> = HashSet::new();

        // First check TraitCall constraints (these have real spans).
        for (trait_name, var, span) in &self.trait_call_constraints {
            let resolved = self.subst.apply(&Ty::Var(*var));
            if matches!(resolved, Ty::Var(_)) {
                continue;
            }
            if let Some(type_name) = ty_canonical_name(&resolved) {
                checked.insert((trait_name.clone(), type_name.clone()));
                if self
                    .impl_env
                    .contains_key(&(trait_name.clone(), type_name.clone()))
                {
                    continue;
                }
                if self.has_param_impl(trait_name, &resolved) {
                    continue;
                }
                return Err(TypeErrorAt::new(
                    TypeError::MissingImpl {
                        trait_name: trait_name.clone(),
                        type_name,
                    },
                    span.clone(),
                ));
            }
        }

        // Then check instantiation-propagated constraints (no precise span).
        for (trait_name, var) in &self.constraint_map {
            let resolved = self.subst.apply(&Ty::Var(*var));
            if matches!(resolved, Ty::Var(_)) {
                continue;
            }
            if let Some(type_name) = ty_canonical_name(&resolved) {
                if checked.contains(&(trait_name.clone(), type_name.clone())) {
                    continue;
                }
                if self
                    .impl_env
                    .contains_key(&(trait_name.clone(), type_name.clone()))
                {
                    continue;
                }
                if self.has_param_impl(trait_name, &resolved) {
                    continue;
                }
                return Err(TypeErrorAt::new(
                    TypeError::MissingImpl {
                        trait_name: trait_name.clone(),
                        type_name,
                    },
                    Span::default(),
                ));
            }
        }

        Ok(())
    }

    /// Check if a parameterized impl can satisfy `trait_name` for `concrete_ty`.
    /// Recursively checks sub-constraints against both concrete and parameterized impls.
    fn has_param_impl(&self, trait_name: &str, concrete_ty: &Ty) -> bool {
        for (p_trait, p_target, p_constraints) in &self.param_impl_env {
            if p_trait != trait_name {
                continue;
            }
            if let Some(bindings) = match_ast_type_against_ty_tc(p_target, concrete_ty) {
                let all_satisfied = p_constraints.iter().all(|(c_trait, c_var)| {
                    if let Some(bound_ty) = bindings.get(c_var) {
                        self.has_impl(c_trait, bound_ty)
                    } else {
                        false
                    }
                });
                if all_satisfied {
                    return true;
                }
            }
        }
        false
    }

    /// Check if any impl (concrete or parameterized) satisfies `trait_name` for `ty`.
    fn has_impl(&self, trait_name: &str, ty: &Ty) -> bool {
        if let Some(type_name) = ty_canonical_name(ty) {
            if self
                .impl_env
                .contains_key(&(trait_name.to_string(), type_name))
            {
                return true;
            }
        }
        self.has_param_impl(trait_name, ty)
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
    checker.arity_env = build_arity_env(&program.items);
    let mut loader = crate::loader::Loader::new();
    let ty = checker.check_program(program, env, path, &mut loader)?;
    // Report the first typed hole as an error (holes are intentional placeholders).
    let mut hole_errors = drain_holes_to_errors(&checker);
    if !hole_errors.is_empty() {
        return Err(hole_errors.remove(0));
    }
    Ok(ty)
}

/// A named type binding for CLI display.
pub struct BindingInfo {
    pub name: String,
    pub scheme: Scheme,
}

// ── Shared helpers for elaborate* functions ───────────────────────────────────

/// Collect the top-level variant names from a pattern.
fn collect_variant_names(pat: &Pattern, out: &mut HashSet<String>) {
    if let Pattern::Variant { name, .. } = pat {
        out.insert(name.clone());
    }
}

/// Convert recorded typed holes into `TypeErrorAt` diagnostics.
/// Must be called after all substitutions are final so the type is resolved.
fn drain_holes_to_errors(checker: &Checker) -> Vec<TypeErrorAt> {
    checker
        .holes
        .iter()
        .filter_map(|(id, span)| {
            checker.node_types.get(id).map(|ty| {
                let ty = checker.subst.apply(ty);
                TypeErrorAt::new(TypeError::TypedHole(ty), span.clone())
            })
        })
        .collect()
}

/// Canonical dict binding name: `Show` + `Num` → `__show_Num`.
/// Spaces in applied types are replaced with `_` so the name is a valid
/// identifier in generated code: `ToText` + `Box Num` → `__totext_Box_Num`.
pub fn impl_dict_name(trait_name: &str, type_name: &str) -> String {
    let sanitized = type_name.replace(' ', "_");
    format!("__{}_{}", trait_name.to_ascii_lowercase(), sanitized)
}

/// Convert a concrete `Ty` to a canonical string matching the key format
/// used in impl registrations.  Returns `None` for type variables.
pub fn ty_canonical_name(ty: &Ty) -> Option<String> {
    match ty {
        Ty::Num => Some("Num".to_string()),
        Ty::Text => Some("Text".to_string()),
        Ty::Bool => Some("Bool".to_string()),
        Ty::Con(name) => Some(name.clone()),
        Ty::App(..) => {
            let (head, args) = ty.flatten_app();
            let head_name = ty_canonical_name(head)?;
            if args.is_empty() {
                Some(head_name)
            } else {
                let arg_strs: Option<Vec<String>> = args.iter().map(|a| ty_canonical_name(a)).collect();
                Some(format!("{} {}", head_name, arg_strs?.join(" ")))
            }
        }
        Ty::Record(row) => {
            if !matches!(row.tail, RowTail::Closed) {
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

/// Synthesize a dictionary-record `Binding` from an `ImplDef` so the type
/// checker can add the dict name to the environment.
///
/// `use Show in Num { show = expr }` → `let __show_Num = { show = expr }`
///
/// When `trait_def` is provided, the binding gets a type annotation that
/// constrains method bodies against the trait's declared signatures with
/// the type parameter substituted for the impl's concrete target type.
fn synth_impl_binding(id: &ImplDef, trait_def: Option<&TraitDef>) -> Binding {
    let dict_name = impl_dict_name(&id.trait_name, &id.type_name);
    let fields: Vec<RecordField> = id
        .methods
        .iter()
        .map(|b| {
            let name = match &b.pattern {
                Pattern::Ident(n, _, _) => n.clone(),
                _ => panic!("impl method patterns must be simple identifiers"),
            };
            RecordField {
                name,
                name_span: crate::error::Span::default(),
                name_node_id: u32::MAX,
                value: Some(b.value.clone()),
            }
        })
        .collect();

    // Build a type annotation from the trait def: substitute the trait's type
    // parameter with the impl's concrete target type in every method signature.
    // When the impl method provides its own type annotation, prefer that over
    // the trait-derived type so the user can constrain the method body precisely.
    let ty = trait_def.map(|td| {
        let ann_fields: Vec<FieldType> = td
            .methods
            .iter()
            .map(|m| {
                // Check if the impl provides a user annotation for this method.
                let user_ty = id.methods.iter().find_map(|b| {
                    let name = match &b.pattern {
                        Pattern::Ident(n, _, _) => n.clone(),
                        _ => return None,
                    };
                    if name == m.name {
                        b.ty.clone()
                    } else {
                        None
                    }
                });
                FieldType {
                    name: m.name.clone(),
                    ty: user_ty
                        .unwrap_or_else(|| subst_type_var(&m.ty, &td.type_param, &id.target_type)),
                }
            })
            .collect();
        Type::Record(RecordType {
            fields: ann_fields,
            open: false,
        })
    });

    Binding {
        pattern: Pattern::Ident(dict_name, crate::error::Span::default(), u32::MAX),
        constraints: id.impl_constraints.clone(),
        ty,
        value: Expr {
            id: u32::MAX,
            kind: ExprKind::Record {
                base: None,
                fields,
                spread: false,
            },
            span: crate::error::Span::default(),
        },
        doc: None,
    }
}

/// Flatten a left-associative AST `App` tree into the base type and a list of arguments.
/// `App(App(Constructor("Result"), Constructor("Num")), Constructor("Text"))`
/// → `(Constructor("Result"), [Constructor("Num"), Constructor("Text")])`.
fn flatten_ast_app(ty: &Type) -> (&Type, Vec<&Type>) {
    let mut args = Vec::new();
    let mut current = ty;
    while let Type::App { callee, arg } = current {
        args.push(arg.as_ref());
        current = callee.as_ref();
    }
    args.reverse();
    (current, args)
}

/// Check whether an AST `Type` references a given type-variable name.
fn ast_type_contains_var(ty: &Type, var_name: &str) -> bool {
    match ty {
        Type::Var(v) => v == var_name,
        Type::Constructor(_) => false,
        Type::App { callee, arg } => {
            ast_type_contains_var(callee, var_name) || ast_type_contains_var(arg, var_name)
        }
        Type::Func { param, ret } => {
            ast_type_contains_var(param, var_name) || ast_type_contains_var(ret, var_name)
        }
        Type::Record(rt) => rt
            .fields
            .iter()
            .any(|f| ast_type_contains_var(&f.ty, var_name)),
    }
}

/// Substitute all occurrences of `Type::Var(var_name)` with `replacement` in `ty`.
fn subst_type_var(ty: &Type, var_name: &str, replacement: &Type) -> Type {
    match ty {
        Type::Var(v) if v == var_name => replacement.clone(),
        Type::Var(_) => ty.clone(),
        Type::Constructor(_) => ty.clone(),
        Type::App { callee, arg } => Type::App {
            callee: Box::new(subst_type_var(callee, var_name, replacement)),
            arg: Box::new(subst_type_var(arg, var_name, replacement)),
        },
        Type::Func { param, ret } => Type::Func {
            param: Box::new(subst_type_var(param, var_name, replacement)),
            ret: Box::new(subst_type_var(ret, var_name, replacement)),
        },
        Type::Record(rt) => Type::Record(RecordType {
            fields: rt
                .fields
                .iter()
                .map(|f| FieldType {
                    name: f.name.clone(),
                    ty: subst_type_var(&f.ty, var_name, replacement),
                })
                .collect(),
            open: rt.open,
        }),
    }
}

/// Match an AST `Type` pattern (with `Type::Var` wildcards) against a concrete
/// `Ty`, returning bindings from variable names to concrete `Ty` values.
fn match_ast_type_against_ty_tc(pattern: &Type, concrete: &Ty) -> Option<HashMap<String, Ty>> {
    let mut bindings = HashMap::new();
    if match_ast_inner_tc(pattern, concrete, &mut bindings) {
        Some(bindings)
    } else {
        None
    }
}

fn match_ast_inner_tc(pattern: &Type, concrete: &Ty, bindings: &mut HashMap<String, Ty>) -> bool {
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
            (_, Ty::Con(c_name)) => name == c_name,
            _ => false,
        },
        (Type::App { .. }, _) => {
            // Flatten both the AST App tree and the Ty App tree, then match pairwise.
            let (ast_base, ast_args) = flatten_ast_app(pattern);
            let (ty_head, ty_args) = concrete.flatten_app();
            if ast_args.len() != ty_args.len() {
                return false;
            }
            if !match_ast_inner_tc(ast_base, ty_head, bindings) {
                return false;
            }
            ast_args
                .iter()
                .zip(ty_args.iter())
                .all(|(p, c)| match_ast_inner_tc(p, c, bindings))
        }
        (Type::Func { param, ret }, Ty::Func(cp, cr)) => {
            match_ast_inner_tc(param, cp, bindings) && match_ast_inner_tc(ret, cr, bindings)
        }
        (Type::Record(rt), Ty::Record(row)) => {
            if rt.fields.len() != row.fields.len() {
                return false;
            }
            if !rt.open && !matches!(row.tail, RowTail::Closed) {
                return false;
            }
            for ast_f in &rt.fields {
                if let Some((_, ty)) = row.fields.iter().find(|(n, _)| n == &ast_f.name) {
                    if !match_ast_inner_tc(&ast_f.ty, ty, bindings) {
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

/// After checking an impl def's synthesized binding, backfill the type of each
/// original method pattern so hover works on impl method names.
fn backfill_impl_method_types(
    id: &ImplDef,
    env: &TypeEnv,
    node_types: &mut HashMap<u32, Ty>,
    subst: &crate::types::Subst,
) {
    let dict_name = impl_dict_name(&id.trait_name, &id.type_name);
    if let Some(scheme) = env.lookup(&dict_name) {
        let ty = subst.apply(&scheme.ty);
        if let Ty::Record(row) = &ty {
            for method in &id.methods {
                if let Pattern::Ident(mname, _, nid) = &method.pattern {
                    if let Some((_, fty)) = row.fields.iter().find(|(n, _)| n == mname) {
                        node_types.insert(*nid, fty.clone());
                    }
                }
            }
        }
    }
}

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
    checker.arity_env = build_arity_env(&program.items);
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
            TopItem::ImplDef(id) => {
                checker.check_impl_completeness(id)?;
                checker.check_impl_annotations(id)?;
                checker.register_impl(id)?;
                let td = checker.trait_env.get(&id.trait_name).cloned();
                let b = synth_impl_binding(id, td.as_ref());
                env = checker.check_binding(&b, env)?;
                backfill_impl_method_types(id, &env, &mut checker.node_types, &checker.subst);
            }
            TopItem::TypeDef(_) => {}
            TopItem::TraitDef(td) => {
                checker.trait_env.insert(td.name.clone(), td.clone());
            }
        }
    }

    checker.check_trait_constraints()?;

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
) -> Result<(HashMap<NodeId, Ty>, Ty, HashMap<NodeId, (String, String)>), TypeErrorAt> {
    let mut subst = Subst::new();
    let (env, mut var_env) = builtin_env(&mut subst);
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::with_subst(var_env, subst);
    checker.arity_env = build_arity_env(&program.items);
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
            TopItem::ImplDef(id) => {
                checker.check_impl_completeness(id)?;
                checker.check_impl_annotations(id)?;
                checker.register_impl(id)?;
                let td = checker.trait_env.get(&id.trait_name).cloned();
                let b = synth_impl_binding(id, td.as_ref());
                env = checker.check_binding(&b, env)?;
                backfill_impl_method_types(id, &env, &mut checker.node_types, &checker.subst);
            }
            TopItem::TypeDef(_) => {}
            TopItem::TraitDef(td) => {
                checker.trait_env.insert(td.name.clone(), td.clone());
            }
        }
    }

    checker.check_trait_constraints()?;

    let export_ty = checker.infer(&env, &program.exports)?;
    let export_ty = checker.subst.apply(&export_ty);

    // Apply the final substitution to every recorded type so all type variables
    // are resolved to their concrete types.
    let resolved_trait_methods = checker.resolved_trait_methods;
    let node_types: HashMap<NodeId, Ty> = checker
        .node_types
        .into_iter()
        .map(|(id, ty)| (id, checker.subst.apply(&ty)))
        .collect();

    Ok((node_types, export_ty, resolved_trait_methods))
}

/// Like [`elaborate`] but also returns the final top-level [`TypeEnv`] so
/// callers (e.g. the LSP) can enumerate all names in scope for completions.
pub fn elaborate_with_env(
    program: &Program,
    path: Option<&Path>,
) -> Result<(HashMap<NodeId, Ty>, TypeEnv, Ty, HashMap<NodeId, (String, String)>), TypeErrorAt> {
    let mut subst = Subst::new();
    let (base_env, mut var_env) = builtin_env(&mut subst);
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::with_subst(var_env, subst);
    checker.arity_env = build_arity_env(&program.items);
    let mut loader = crate::loader::Loader::new();
    let mut env = base_env;

    checker.apply_imports(&program.uses, path, &mut loader, &mut env)?;

    // Validate trait definitions up front (method must reference type param),
    // but do NOT register them yet — traits are registered sequentially.
    for item in &program.items {
        if let TopItem::TraitDef(td) = item {
            for method in &td.methods {
                if !ast_type_contains_var(&method.ty, &td.type_param) {
                    return Err(TypeErrorAt::new(
                        TypeError::TraitMethodMissingParam {
                            trait_name: td.name.clone(),
                            method_name: method.name.clone(),
                            type_param: td.type_param.clone(),
                        },
                        method.name_span.clone(),
                    ));
                }
            }
        }
    }

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
            TopItem::ImplDef(id) => {
                checker.check_impl_completeness(id)?;
                checker.check_impl_annotations(id)?;
                checker.register_impl(id)?;
                let td = checker.trait_env.get(&id.trait_name).cloned();
                let b = synth_impl_binding(id, td.as_ref());
                env = checker.check_binding(&b, env)?;
                backfill_impl_method_types(id, &env, &mut checker.node_types, &checker.subst);
            }
            TopItem::TypeDef(_) => {}
            TopItem::TraitDef(td) => {
                checker.trait_env.insert(td.name.clone(), td.clone());
            }
        }
    }

    checker.check_trait_constraints()?;

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
    // Report the first typed hole as an error (must happen before node_types is moved).
    let mut hole_errors = drain_holes_to_errors(&checker);

    let resolved_trait_methods = checker.resolved_trait_methods;
    let node_types: HashMap<NodeId, Ty> = checker
        .node_types
        .into_iter()
        .map(|(id, ty)| (id, checker.subst.apply(&ty)))
        .collect();

    if !hole_errors.is_empty() {
        return Err(hole_errors.remove(0));
    }

    Ok((node_types, resolved_env, export_ty, resolved_trait_methods))
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
) -> (
    HashMap<NodeId, Ty>,
    TypeEnv,
    HashMap<String, TraitDef>,
    Vec<TypeErrorAt>,
) {
    let mut subst = Subst::new();
    let (base_env, mut var_env) = builtin_env(&mut subst);
    let prog_vars = build_variant_env(&program.items);
    var_env.merge(prog_vars);
    let mut checker = Checker::with_subst(var_env, subst);
    checker.arity_env = build_arity_env(&program.items);
    let mut loader = crate::loader::Loader::new();
    let mut env = base_env;

    let mut errors = checker.apply_imports_partial(&program.uses, path, &mut loader, &mut env);

    // Validate trait definitions up front (method must reference type param),
    // but do NOT register them yet — traits are registered sequentially.
    for item in &program.items {
        if let TopItem::TraitDef(td) = item {
            for method in &td.methods {
                if !ast_type_contains_var(&method.ty, &td.type_param) {
                    errors.push(TypeErrorAt::new(
                        TypeError::TraitMethodMissingParam {
                            trait_name: td.name.clone(),
                            method_name: method.name.clone(),
                            type_param: td.type_param.clone(),
                        },
                        method.name_span.clone(),
                    ));
                }
            }
        }
    }

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
            TopItem::ImplDef(id) => {
                if let Err(e) = checker.check_impl_completeness(id) {
                    errors.push(e);
                }
                if let Err(e) = checker.check_impl_annotations(id) {
                    errors.push(e);
                }
                if let Err(e) = checker.register_impl(id) {
                    errors.push(e);
                }
                let td = checker.trait_env.get(&id.trait_name).cloned();
                let b = synth_impl_binding(id, td.as_ref());
                match checker.check_binding(&b, env.clone()) {
                    Ok(new_env) => {
                        env = new_env;
                        backfill_impl_method_types(
                            id,
                            &env,
                            &mut checker.node_types,
                            &checker.subst,
                        );
                    }
                    Err(e) => errors.push(e),
                }
            }
            TopItem::TypeDef(_) => {}
            TopItem::TraitDef(td) => {
                checker.trait_env.insert(td.name.clone(), td.clone());
            }
        }
    }

    // Collect missing-impl errors (non-fatal for LSP).
    if let Err(e) = checker.check_trait_constraints() {
        errors.push(e);
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
        .iter()
        .map(|(id, ty)| (*id, checker.subst.apply(ty)))
        .collect();

    // Include typed holes as diagnostics so the LSP can report them.
    let mut hole_errors = drain_holes_to_errors(&checker);
    errors.append(&mut hole_errors);

    (node_types, resolved_env, checker.trait_env, errors)
}
