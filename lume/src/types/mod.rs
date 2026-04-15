pub mod error;
pub mod infer;

pub use error::{TypeError, TypeErrorAt};

use std::collections::{HashMap, HashSet};
use std::fmt;

// TyVar is just a cheap integer handle.  Every unification variable in the
// program gets a unique ID from the shared counter in Subst.
pub type TyVar = u32;

// ── Core types ────────────────────────────────────────────────────────────────

// The internal representation of a Lume type.
//
// Ground types (Num, Text, Bool) carry no information - equality is structural.
// Compound types (List, Func, Con) recurse into their children.
// Record wraps a Row (see below) to carry the field / open-tail information.
// Var is an *unification variable*: a placeholder that the solver will
//   eventually bind to a concrete type.  These are never written in user code;
//   they are generated internally by `Subst::fresh_var`.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Num,
    Text,
    Bool,
    List(Box<Ty>),
    /// Single-argument functions only; multi-argument functions are curried:
    /// `a -> b -> c` is stored as `Func(a, Func(b, c))`.
    Func(Box<Ty>, Box<Ty>),
    Record(Row),
    /// User-defined type constructor with type arguments.
    /// `Shape` → `Con("Shape", [])`, `Tree Num` → `Con("Tree", [Num])`.
    Con(String, Vec<Ty>),
    /// An unsolved unification variable.  Once the solver binds it via
    /// `Subst::bind_ty`, every future call to `apply` replaces it.
    Var(TyVar),
}

// Row types are the mechanism behind record / structural typing.
//
// A Row describes the fields of a record.  The tail decides whether the record
// is "closed" (exact field set) or "open" (more fields allowed):
//
//   { x: Num }           →  Row { fields:[("x",Num)], tail:Closed }
//   { x: Num, ..r }      →  Row { fields:[("x",Num)], tail:Open(r) }
//
// `Open(v)` is itself an unification variable (row variable).  Unifying two
// open rows creates a fresh shared tail that captures the union of extra
// fields - this is what makes row polymorphism work.
//
// Fields are always kept sorted by name so that structural equality and
// display are order-independent.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Field pairs, kept sorted by name.
    pub fields: Vec<(String, Ty)>,
    pub tail: RowTail,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowTail {
    /// No more fields - the record is fully determined.
    Closed,
    /// More fields may be added later; `v` is an unbound row variable.
    Open(TyVar),
}

// A polymorphic type scheme  ∀ vars row_vars . ty
//
// `vars` are the quantified *type* variables; `row_vars` are the quantified
// *row* variables.  Both start as ordinary TyVar integers - the quantification
// is only by membership in these lists, not by a separate namespace.
//
// A monomorphic type has empty `vars` and `row_vars`.  `Scheme::mono` is a
// convenience constructor for that common case.
//
// Schemes live in the TypeEnv.  When a name is used, its scheme is
// *instantiated* (all quantified vars are replaced with fresh ones) so that
// each use site gets an independent set of type variables.
#[derive(Debug, Clone)]
pub struct Scheme {
    pub vars: Vec<TyVar>,
    pub row_vars: Vec<TyVar>,
    /// Trait constraints on quantified type variables: `(trait_name, var)`.
    pub constraints: Vec<(String, TyVar)>,
    pub ty: Ty,
}

impl Scheme {
    pub fn mono(ty: Ty) -> Self {
        Scheme {
            vars: vec![],
            row_vars: vec![],
            constraints: vec![],
            ty,
        }
    }
}

// ── Substitution ──────────────────────────────────────────────────────────────

// The Subst is the *entire* mutable state of one type-checking session.
//
// It plays three roles simultaneously:
//
//  1. Fresh-variable allocator - `counter` is bumped on every `fresh_var`.
//
//  2. Type-variable binding map - `tys` maps solved type variables to their
//     concrete types.  Before a variable is solved it simply isn't present.
//     Think of it as the "find" table of a union-find structure.
//
//  3. Row-variable binding map - `rows` does the same for row variables.
//     The value is a partial Row (fields + new tail) rather than a full type.
//
// One Subst is created per module being type-checked.  All checkers for
// transitive imports use their own Subst; the export type is fully normalised
// before being packaged into a Scheme, so no Subst leaks across module
// boundaries.
#[derive(Debug, Clone, Default)]
pub struct Subst {
    pub counter: u32,
    tys: HashMap<TyVar, Ty>,
    rows: HashMap<TyVar, Row>,
}

impl Subst {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh type variable.  The returned ID is guaranteed not to
    /// have been used before in this Subst session.
    pub fn fresh_var(&mut self) -> TyVar {
        let v = self.counter;
        self.counter += 1;
        v
    }

    pub fn fresh_ty(&mut self) -> Ty {
        Ty::Var(self.fresh_var())
    }

    // Record the decision that unification variable `v` equals `ty`.
    //
    // Invariants maintained:
    //
    //  • No self-loops: if `apply(ty) == Var(v)` the mapping would create a
    //    trivial cycle; we drop it.
    //
    //  • No redundant overwrites: if `v` is already bound, the new type must
    //    be *consistent* with the existing one, so we delegate to `unify`
    //    rather than blindly overwriting.  Overwriting is the classic source
    //    of "phantom" cycles where previously-solved information is lost.
    //
    //  • Normalisation before insertion: we apply the current substitution to
    //    `ty` first so that the stored value is as ground as possible.  This
    //    keeps chains short and prevents false cycles from path compression.
    pub fn bind_ty(&mut self, v: TyVar, ty: Ty) {
        // Normalize through the current substitution so chains that already
        // lead back to `v` are collapsed before we insert anything.
        let ty = self.apply(&ty);
        if ty == Ty::Var(v) {
            return; // self-loop / already equivalent - nothing to do
        }
        if let Some(existing) = self.tys.get(&v).cloned() {
            // v is already solved; the two solutions must agree.
            let _ = unify(self, existing, ty);
        } else {
            self.tys.insert(v, ty);
        }
    }

    // Same contract as `bind_ty` but for row variables.
    //
    // A row self-loop looks like: Open(v) with no extra fields and tail=Open(v),
    // i.e. the row variable points back to itself with nothing new.
    pub fn bind_row(&mut self, v: TyVar, row: Row) {
        let row = self.apply_row(&row);
        if row.tail == RowTail::Open(v) && row.fields.is_empty() {
            return; // self-loop
        }
        if let Some(existing) = self.rows.get(&v).cloned() {
            let _ = unify_rows(self, existing, row);
        } else {
            self.rows.insert(v, row);
        }
    }

    // Walk a type to its fully-normalised form.
    //
    // For `Var(v)`: follow the chain `v → tys[v] → tys[tys[v]] → …` until we
    // reach either a non-Var type or an unbound variable.  This is the "find"
    // step of the union-find.
    //
    // For structural types: recurse into children so the entire tree is
    // normalised.  We do *not* increment a depth counter for structural
    // recursion - only Var-chain following matters for cycle detection.
    // Cycles in `tys` are prevented by `bind_ty`; once that invariant holds,
    // `apply` always terminates.
    pub fn apply(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(v) => match self.tys.get(v) {
                Some(t) => self.apply(t),
                None => Ty::Var(*v),
            },
            Ty::List(t) => Ty::List(Box::new(self.apply(t))),
            Ty::Func(a, b) => Ty::Func(Box::new(self.apply(a)), Box::new(self.apply(b))),
            Ty::Record(row) => Ty::Record(self.apply_row(row)),
            Ty::Con(n, args) => Ty::Con(n.clone(), args.iter().map(|a| self.apply(a)).collect()),
            _ => ty.clone(),
        }
    }

    // Normalise a row by following the row-variable chain and collecting all
    // accumulated fields.
    //
    // The row chain is a linked list: each Open(v) node may have a binding in
    // `self.rows` that extends it with more fields and a new tail.  We walk
    // the chain (iteratively, not recursively, to stay stack-safe) merging
    // every extension into a single flat field list.  Earlier fields win if
    // the same name appears twice (the binder closest to the head of the
    // chain is the most recent constraint).
    //
    // The result always has a canonical sorted field list and either a Closed
    // tail or an Open(v) where v is unbound.
    pub fn apply_row(&self, row: &Row) -> Row {
        let mut fields: Vec<(String, Ty)> = row
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), self.apply(v)))
            .collect();
        let mut tail = row.tail.clone();

        loop {
            match tail.clone() {
                RowTail::Closed => break,
                RowTail::Open(v) => match self.rows.get(&v) {
                    None => break, // unbound - chain ends here
                    Some(ext) => {
                        // Merge extension fields; earlier (more-derived) fields take priority.
                        for (k, t) in &ext.fields {
                            if !fields.iter().any(|(fk, _)| fk == k) {
                                fields.push((k.clone(), self.apply(t)));
                            }
                        }
                        tail = ext.tail.clone();
                    }
                },
            }
        }
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        Row { fields, tail }
    }

    // Apply the substitution to a scheme while *skipping* the quantified vars.
    //
    // Quantified vars (those in `scheme.vars` / `scheme.row_vars`) are
    // placeholders that will be replaced at every *use site* by fresh vars.
    // They must not be substituted away by the ambient substitution - doing so
    // would corrupt the scheme's polymorphism.
    //
    // We implement the skip by building a restricted Subst that excludes the
    // quantified vars from the binding maps.  The restricted Subst is
    // temporary and only used for this one `apply` call.
    pub fn apply_scheme(&self, scheme: &Scheme) -> Scheme {
        let tys: HashMap<TyVar, Ty> = self
            .tys
            .iter()
            .filter(|(k, _)| !scheme.vars.contains(k))
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let rows: HashMap<TyVar, Row> = self
            .rows
            .iter()
            .filter(|(k, _)| !scheme.row_vars.contains(k))
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let restricted = Subst {
            counter: self.counter,
            tys,
            rows,
        };
        Scheme {
            vars: scheme.vars.clone(),
            row_vars: scheme.row_vars.clone(),
            constraints: scheme.constraints.clone(),
            ty: restricted.apply(&scheme.ty),
        }
    }
}

// ── Free variable collection ──────────────────────────────────────────────────

// These two functions collect the *syntactic* free variables of a (possibly
// partially-solved) type - i.e. the Var nodes that remain after every chain
// has been followed as far as it goes.
//
// They are used by `generalise` to decide which variables are safe to
// quantify: a variable that still appears free in the *environment* is a
// monomorphic "skolem" and must not be generalised.

pub fn free_type_vars(ty: &Ty) -> HashSet<TyVar> {
    let mut set = HashSet::new();
    collect_ftv(ty, &mut set);
    set
}

pub fn free_row_vars(ty: &Ty) -> HashSet<TyVar> {
    let mut set = HashSet::new();
    collect_frv(ty, &mut set);
    set
}

fn collect_ftv(ty: &Ty, set: &mut HashSet<TyVar>) {
    match ty {
        Ty::Var(v) => {
            set.insert(*v);
        }
        Ty::List(t) => collect_ftv(t, set),
        Ty::Func(a, b) => {
            collect_ftv(a, set);
            collect_ftv(b, set);
        }
        Ty::Record(r) => r.fields.iter().for_each(|(_, t)| collect_ftv(t, set)),
        Ty::Con(_, args) => args.iter().for_each(|a| collect_ftv(a, set)),
        _ => {}
    }
}

// Row variables (Open tails) are tracked separately because they participate
// in a different part of the scheme: `Scheme::row_vars` vs `Scheme::vars`.
fn collect_frv(ty: &Ty, set: &mut HashSet<TyVar>) {
    match ty {
        Ty::List(t) => collect_frv(t, set),
        Ty::Func(a, b) => {
            collect_frv(a, set);
            collect_frv(b, set);
        }
        Ty::Record(r) => {
            r.fields.iter().for_each(|(_, t)| collect_frv(t, set));
            if let RowTail::Open(v) = r.tail {
                set.insert(v);
            }
        }
        Ty::Con(_, args) => args.iter().for_each(|a| collect_frv(a, set)),
        _ => {}
    }
}

// ── Unification ───────────────────────────────────────────────────────────────

// Robinson-style first-order unification.
//
// Given two types `t1` and `t2`, find the most-general substitution that
// makes them equal.  The substitution is accumulated destructively in `s`.
//
// Steps:
//  1. Normalise both types through the current substitution first - so we
//     always work with the most-ground versions, not stale Var pointers.
//  2. If both are the same ground type (Num/Text/Bool), nothing to do.
//  3. If either is a Var:
//      a. If it's the same var, trivially equal.
//      b. Run the occurs check: binding `v` to a type that contains `v`
//         itself would create an infinite type (e.g. `a = List a`).  Reject.
//      c. Record the binding via `bind_ty`.
//  4. Structural cases (List, Func, Con): unify children pairwise.
//  5. Records delegate to `unify_rows`.
//  6. Anything else: mismatch error.
pub fn unify(s: &mut Subst, t1: Ty, t2: Ty) -> Result<(), TypeError> {
    let t1 = s.apply(&t1);
    let t2 = s.apply(&t2);

    match (t1, t2) {
        (Ty::Num, Ty::Num) | (Ty::Text, Ty::Text) | (Ty::Bool, Ty::Bool) => Ok(()),

        (Ty::Var(v), t) => {
            if t == Ty::Var(v) {
                return Ok(()); // already the same variable
            }
            if ty_occurs(v, &t) {
                return Err(TypeError::OccursCheck(v));
            }
            s.bind_ty(v, t);
            Ok(())
        }
        (t, Ty::Var(v)) => {
            if ty_occurs(v, &t) {
                return Err(TypeError::OccursCheck(v));
            }
            s.bind_ty(v, t);
            Ok(())
        }

        (Ty::List(a), Ty::List(b)) => unify(s, *a, *b),

        // Functions unify contra-co-variantly: param types must match and
        // return types must match.  (Lume has no subtyping so both are
        // invariant in practice.)
        (Ty::Func(p1, r1), Ty::Func(p2, r2)) => {
            unify(s, *p1, *p2)?;
            unify(s, *r1, *r2)
        }

        (Ty::Record(r1), Ty::Record(r2)) => unify_rows(s, r1, r2),

        // Nominal: two Con types unify only if they have the same constructor
        // name and the same arity, then their arguments are unified pairwise.
        (Ty::Con(n1, a1), Ty::Con(n2, a2)) if n1 == n2 && a1.len() == a2.len() => {
            for (t1, t2) in a1.into_iter().zip(a2.into_iter()) {
                unify(s, t1, t2)?;
            }
            Ok(())
        }

        (t1, t2) => Err(TypeError::Mismatch(t1, t2)),
    }
}

// Row unification - the core of row polymorphism.
//
// Intuition: a row represents a *set* of field constraints.  Unifying two
// rows means finding an assignment of the tail variables that satisfies both
// sets simultaneously.
//
// Algorithm (after normalising both sides):
//
//  1. Collect common fields and unify them pairwise.
//  2. Collect the fields that appear on only one side ("extras").
//  3. Dispatch on the tail combination:
//
//     Closed × Closed  - No extras allowed; exact match required.
//
//     Open(v) × Closed - The open side must not have fields the closed side
//                         lacks (that would violate the closed constraint).
//                         The open tail absorbs the closed side's extras:
//                           bind v → { extras_from_closed | Closed }
//
//     Closed × Open(v) - Symmetric.
//
//     Open(v1) × Open(v2), v1 ≠ v2 - Neither side is fully known.  Create a
//                         fresh shared tail `f` and bind:
//                           v1 → { extras_from_r2 | Open(f) }
//                           v2 → { extras_from_r1 | Open(f) }
//                         This encodes: "both rows must have all the fields
//                         of the other, plus whatever `f` brings."
//
//     Open(v) × Open(v) - Same variable: no extras allowed (they're already
//                         structurally equal once we normalised).
fn unify_rows(s: &mut Subst, r1: Row, r2: Row) -> Result<(), TypeError> {
    let r1 = s.apply_row(&r1);
    let r2 = s.apply_row(&r2);

    let map1: HashMap<&str, &Ty> = r1.fields.iter().map(|(k, v)| (k.as_str(), v)).collect();
    let map2: HashMap<&str, &Ty> = r2.fields.iter().map(|(k, v)| (k.as_str(), v)).collect();

    // Unify shared fields; collect field-set differences.
    let mut extras1: Vec<(String, Ty)> = Vec::new();
    let mut extras2: Vec<(String, Ty)> = Vec::new();

    for (k, t1) in &r1.fields {
        match map2.get(k.as_str()) {
            Some(t2) => unify(s, t1.clone(), (*t2).clone())?,
            None => extras1.push((k.clone(), t1.clone())),
        }
    }
    for (k, t2) in &r2.fields {
        if !map1.contains_key(k.as_str()) {
            extras2.push((k.clone(), t2.clone()));
        }
    }

    match (r1.tail, r2.tail) {
        (RowTail::Closed, RowTail::Closed) => {
            // Both closed - field sets must be identical.
            let bad = extras1.first().or(extras2.first()).map(|(f, _)| f.clone());
            if let Some(f) = bad {
                return Err(TypeError::RowMismatch(f));
            }
        }

        (RowTail::Open(v1), RowTail::Closed) => {
            // r1 is open; r2 is closed.
            // r1 must not have fields that r2 can't accommodate.
            if let Some((f, _)) = extras1.first() {
                return Err(TypeError::RowMismatch(f.clone()));
            }
            // r1's row variable absorbs the extra fields from r2.
            let new_row = Row {
                fields: extras2,
                tail: RowTail::Closed,
            };
            if row_var_occurs(v1, &new_row) {
                return Err(TypeError::OccursCheck(v1));
            }
            s.bind_row(v1, new_row);
        }

        (RowTail::Closed, RowTail::Open(v2)) => {
            // Symmetric.
            if let Some((f, _)) = extras2.first() {
                return Err(TypeError::RowMismatch(f.clone()));
            }
            let new_row = Row {
                fields: extras1,
                tail: RowTail::Closed,
            };
            if row_var_occurs(v2, &new_row) {
                return Err(TypeError::OccursCheck(v2));
            }
            s.bind_row(v2, new_row);
        }

        (RowTail::Open(v1), RowTail::Open(v2)) => {
            if v1 == v2 {
                // Same row var - no extras allowed.
                let bad = extras1.first().or(extras2.first()).map(|(f, _)| f.clone());
                if let Some(f) = bad {
                    return Err(TypeError::RowMismatch(f));
                }
            } else {
                // Different row vars: create a fresh shared tail.
                let fresh = s.fresh_var();
                let r1_ext = Row {
                    fields: extras2,
                    tail: RowTail::Open(fresh),
                };
                let r2_ext = Row {
                    fields: extras1,
                    tail: RowTail::Open(fresh),
                };
                if row_var_occurs(v1, &r1_ext) || row_var_occurs(v2, &r2_ext) {
                    return Err(TypeError::OccursCheck(v1));
                }
                s.bind_row(v1, r1_ext);
                s.bind_row(v2, r2_ext);
            }
        }
    }
    Ok(())
}

// The occurs check prevents binding `v` to any type that contains `v`.
// Without it, `unify(a, List a)` would succeed and create an infinite type,
// causing `apply` to loop forever when it tried to normalise `a`.
fn ty_occurs(v: TyVar, ty: &Ty) -> bool {
    match ty {
        Ty::Var(u) => *u == v,
        Ty::List(t) => ty_occurs(v, t),
        Ty::Func(a, b) => ty_occurs(v, a) || ty_occurs(v, b),
        Ty::Record(r) => {
            r.fields.iter().any(|(_, t)| ty_occurs(v, t)) || r.tail == RowTail::Open(v)
        }
        Ty::Con(_, args) => args.iter().any(|a| ty_occurs(v, a)),
        _ => false,
    }
}

// Row-variable occurs check: `v` must not appear in the tail or in any field
// type of `row`, otherwise binding v → row would create a cycle in the row
// chain (analogous to infinite types for regular variables).
fn row_var_occurs(v: TyVar, row: &Row) -> bool {
    row.tail == RowTail::Open(v) || row.fields.iter().any(|(_, t)| ty_occurs(v, t))
}

// ── Display ───────────────────────────────────────────────────────────────────

// ── Pretty variable names ─────────────────────────────────────────────────────

/// Map the i-th type/row variable to a name: a, b, …, z, a1, b1, …
fn pretty_var_name(i: usize) -> String {
    let letter = (b'a' + (i % 26) as u8) as char;
    if i < 26 {
        letter.to_string()
    } else {
        format!("{}{}", letter, i / 26)
    }
}

/// Collect free type-vars and row-vars in first-appearance order.
fn collect_pretty_vars(ty: &Ty, tvs: &mut Vec<TyVar>, rvs: &mut Vec<TyVar>) {
    match ty {
        Ty::Var(v) => {
            if !tvs.contains(v) {
                tvs.push(*v);
            }
        }
        Ty::List(t) => collect_pretty_vars(t, tvs, rvs),
        Ty::Func(a, b) => {
            collect_pretty_vars(a, tvs, rvs);
            collect_pretty_vars(b, tvs, rvs);
        }
        Ty::Record(row) => {
            for (_, t) in &row.fields {
                collect_pretty_vars(t, tvs, rvs);
            }
            if let RowTail::Open(v) = row.tail {
                if !rvs.contains(&v) {
                    rvs.push(v);
                }
            }
        }
        Ty::Con(_, args) => {
            for a in args {
                collect_pretty_vars(a, tvs, rvs);
            }
        }
        _ => {}
    }
}

/// Render a `Ty` using caller-supplied name tables for type-vars and row-vars.
fn fmt_named(ty: &Ty, tvs: &[TyVar], rvs: &[TyVar], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match ty {
        Ty::Num => write!(f, "Num"),
        Ty::Text => write!(f, "Text"),
        Ty::Bool => write!(f, "Bool"),
        Ty::List(t) => {
            write!(f, "List ")?;
            fmt_named_atomic(t, tvs, rvs, f)
        }
        Ty::Func(a, b) => {
            if matches!(a.as_ref(), Ty::Func(..)) {
                write!(f, "(")?;
                fmt_named(a, tvs, rvs, f)?;
                write!(f, ")")?;
            } else {
                fmt_named(a, tvs, rvs, f)?;
            }
            write!(f, " -> ")?;
            fmt_named(b, tvs, rvs, f)
        }
        Ty::Record(row) => {
            write!(f, "{{ ")?;
            for (i, (name, ty)) in row.fields.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}: ", name)?;
                fmt_named(ty, tvs, rvs, f)?;
            }
            if let RowTail::Open(v) = row.tail {
                if !row.fields.is_empty() {
                    write!(f, ", ")?;
                }
                let name = rvs
                    .iter()
                    .position(|x| x == &v)
                    .map(|i| pretty_var_name(tvs.len() + i))
                    .unwrap_or_else(|| format!("?{}", v));
                write!(f, "..{}", name)?;
            }
            write!(f, " }}")
        }
        Ty::Con(name, args) if args.is_empty() => write!(f, "{}", name),
        Ty::Con(name, args) => {
            write!(f, "{}", name)?;
            for a in args {
                write!(f, " ")?;
                fmt_named_atomic(a, tvs, rvs, f)?;
            }
            Ok(())
        }
        Ty::Var(v) => {
            let name = tvs
                .iter()
                .position(|x| x == v)
                .map(pretty_var_name)
                .unwrap_or_else(|| format!("?{}", v));
            write!(f, "{}", name)
        }
    }
}

fn fmt_named_atomic(
    ty: &Ty,
    tvs: &[TyVar],
    rvs: &[TyVar],
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    let needs_parens =
        matches!(ty, Ty::Func(..)) || matches!(ty, Ty::Con(_, args) if !args.is_empty());
    if needs_parens {
        write!(f, "(")?;
        fmt_named(ty, tvs, rvs, f)?;
        write!(f, ")")
    } else {
        fmt_named(ty, tvs, rvs, f)
    }
}

// ── Display impls ─────────────────────────────────────────────────────────────

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut tvs = Vec::new();
        let mut rvs = Vec::new();
        collect_pretty_vars(self, &mut tvs, &mut rvs);
        fmt_named(self, &tvs, &rvs, f)
    }
}

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Delegate to Ty::Display via a temporary record type.
        write!(f, "{}", Ty::Record(self.clone()))
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.constraints.is_empty() {
            write!(f, "(")?;
            for (i, (trait_name, var)) in self.constraints.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                let var_name = self
                    .vars
                    .iter()
                    .position(|v| v == var)
                    .map(pretty_var_name)
                    .unwrap_or_else(|| format!("?{}", var));
                write!(f, "{} {}", trait_name, var_name)?;
            }
            write!(f, ") => ")?;
        }
        fmt_named(&self.ty, &self.vars, &self.row_vars, f)
    }
}
