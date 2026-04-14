pub mod error;
pub mod infer;

pub use error::{TypeError, TypeErrorAt};

use std::collections::{HashMap, HashSet};
use std::fmt;

pub type TyVar = u32;

// ── Core types ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Num,
    Text,
    Bool,
    List(Box<Ty>),
    Func(Box<Ty>, Box<Ty>),
    Record(Row),
    /// User-defined: `Shape`, `Tree Num`, `Maybe Text`
    Con(String, Vec<Ty>),
    /// Unification variable
    Var(TyVar),
}

/// The "inside" of a record type.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Field pairs, kept sorted by name.
    pub fields: Vec<(String, Ty)>,
    pub tail: RowTail,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowTail {
    Closed,
    Open(TyVar),
}

/// A polymorphic type scheme ∀vars row_vars. ty
#[derive(Debug, Clone)]
pub struct Scheme {
    pub vars: Vec<TyVar>,
    pub row_vars: Vec<TyVar>,
    pub ty: Ty,
}

impl Scheme {
    pub fn mono(ty: Ty) -> Self {
        Scheme { vars: vec![], row_vars: vec![], ty }
    }
}

// ── Substitution ──────────────────────────────────────────────────────────────

/// The shared mutable state of the type inference engine:
/// a fresh-variable counter plus two union-find tables.
#[derive(Debug, Clone, Default)]
pub struct Subst {
    pub counter: u32,
    tys: HashMap<TyVar, Ty>,
    rows: HashMap<TyVar, Row>,
}

impl Subst {
    pub fn new() -> Self { Self::default() }

    pub fn fresh_var(&mut self) -> TyVar {
        let v = self.counter;
        self.counter += 1;
        v
    }

    pub fn fresh_ty(&mut self) -> Ty { Ty::Var(self.fresh_var()) }

    pub fn bind_ty(&mut self, v: TyVar, ty: Ty) { self.tys.insert(v, ty); }
    pub fn bind_row(&mut self, v: TyVar, row: Row) { self.rows.insert(v, row); }

    /// Apply substitution to a type (normalise, following chains).
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

    /// Apply substitution to a row, following row-variable chains and
    /// merging fields from each step in the chain.
    pub fn apply_row(&self, row: &Row) -> Row {
        let mut fields: Vec<(String, Ty)> = row.fields.iter()
            .map(|(k, v)| (k.clone(), self.apply(v)))
            .collect();
        let mut tail = row.tail.clone();

        loop {
            match tail.clone() {
                RowTail::Closed => break,
                RowTail::Open(v) => match self.rows.get(&v) {
                    None => break,
                    Some(ext) => {
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

    /// Apply substitution to a scheme, skipping quantified variables.
    pub fn apply_scheme(&self, scheme: &Scheme) -> Scheme {
        let tys: HashMap<TyVar, Ty> = self.tys.iter()
            .filter(|(k, _)| !scheme.vars.contains(k))
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let rows: HashMap<TyVar, Row> = self.rows.iter()
            .filter(|(k, _)| !scheme.row_vars.contains(k))
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let restricted = Subst { counter: self.counter, tys, rows };
        Scheme {
            vars: scheme.vars.clone(),
            row_vars: scheme.row_vars.clone(),
            ty: restricted.apply(&scheme.ty),
        }
    }
}

// ── Free variable collection ──────────────────────────────────────────────────

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
        Ty::Var(v) => { set.insert(*v); }
        Ty::List(t) => collect_ftv(t, set),
        Ty::Func(a, b) => { collect_ftv(a, set); collect_ftv(b, set); }
        Ty::Record(r) => r.fields.iter().for_each(|(_, t)| collect_ftv(t, set)),
        Ty::Con(_, args) => args.iter().for_each(|a| collect_ftv(a, set)),
        _ => {}
    }
}

fn collect_frv(ty: &Ty, set: &mut HashSet<TyVar>) {
    match ty {
        Ty::List(t) => collect_frv(t, set),
        Ty::Func(a, b) => { collect_frv(a, set); collect_frv(b, set); }
        Ty::Record(r) => {
            r.fields.iter().for_each(|(_, t)| collect_frv(t, set));
            if let RowTail::Open(v) = r.tail { set.insert(v); }
        }
        Ty::Con(_, args) => args.iter().for_each(|a| collect_frv(a, set)),
        _ => {}
    }
}

// ── Unification ───────────────────────────────────────────────────────────────

pub fn unify(s: &mut Subst, t1: Ty, t2: Ty) -> Result<(), TypeError> {
    let t1 = s.apply(&t1);
    let t2 = s.apply(&t2);

    match (t1, t2) {
        (Ty::Num, Ty::Num) | (Ty::Text, Ty::Text) | (Ty::Bool, Ty::Bool) => Ok(()),

        (Ty::Var(v), t) => {
            if t == Ty::Var(v) { return Ok(()); }
            if ty_occurs(v, &t) { return Err(TypeError::OccursCheck(v)); }
            s.bind_ty(v, t);
            Ok(())
        }
        (t, Ty::Var(v)) => {
            if ty_occurs(v, &t) { return Err(TypeError::OccursCheck(v)); }
            s.bind_ty(v, t);
            Ok(())
        }

        (Ty::List(a), Ty::List(b)) => unify(s, *a, *b),

        (Ty::Func(p1, r1), Ty::Func(p2, r2)) => {
            unify(s, *p1, *p2)?;
            unify(s, *r1, *r2)
        }

        (Ty::Record(r1), Ty::Record(r2)) => unify_rows(s, r1, r2),

        (Ty::Con(n1, a1), Ty::Con(n2, a2)) if n1 == n2 && a1.len() == a2.len() => {
            for (t1, t2) in a1.into_iter().zip(a2.into_iter()) {
                unify(s, t1, t2)?;
            }
            Ok(())
        }

        (t1, t2) => Err(TypeError::Mismatch(t1, t2)),
    }
}

/// Row unification — the core of row polymorphism.
///
/// After normalising both rows:
/// - Common fields are unified pairwise.
/// - Extra fields from each side are absorbed by the other row's tail variable.
/// - If both are closed, field sets must match exactly.
/// - If both are open with different row vars, a fresh shared tail is created.
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
            // Both closed — field sets must be identical.
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
            let new_row = Row { fields: extras2, tail: RowTail::Closed };
            if row_var_occurs(v1, &new_row) { return Err(TypeError::OccursCheck(v1)); }
            s.bind_row(v1, new_row);
        }

        (RowTail::Closed, RowTail::Open(v2)) => {
            // Symmetric.
            if let Some((f, _)) = extras2.first() {
                return Err(TypeError::RowMismatch(f.clone()));
            }
            let new_row = Row { fields: extras1, tail: RowTail::Closed };
            if row_var_occurs(v2, &new_row) { return Err(TypeError::OccursCheck(v2)); }
            s.bind_row(v2, new_row);
        }

        (RowTail::Open(v1), RowTail::Open(v2)) => {
            if v1 == v2 {
                // Same row var — no extras allowed.
                let bad = extras1.first().or(extras2.first()).map(|(f, _)| f.clone());
                if let Some(f) = bad { return Err(TypeError::RowMismatch(f)); }
            } else {
                // Different row vars: create a fresh shared tail.
                let fresh = s.fresh_var();
                let r1_ext = Row { fields: extras2, tail: RowTail::Open(fresh) };
                let r2_ext = Row { fields: extras1, tail: RowTail::Open(fresh) };
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

fn ty_occurs(v: TyVar, ty: &Ty) -> bool {
    match ty {
        Ty::Var(u) => *u == v,
        Ty::List(t) => ty_occurs(v, t),
        Ty::Func(a, b) => ty_occurs(v, a) || ty_occurs(v, b),
        Ty::Record(r) => r.fields.iter().any(|(_, t)| ty_occurs(v, t)) || r.tail == RowTail::Open(v),
        Ty::Con(_, args) => args.iter().any(|a| ty_occurs(v, a)),
        _ => false,
    }
}

fn row_var_occurs(v: TyVar, row: &Row) -> bool {
    row.tail == RowTail::Open(v) || row.fields.iter().any(|(_, t)| ty_occurs(v, t))
}

// ── Display ───────────────────────────────────────────────────────────────────

// ── Pretty variable names ─────────────────────────────────────────────────────

/// Map the i-th type/row variable to a name: a, b, …, z, a1, b1, …
fn pretty_var_name(i: usize) -> String {
    let letter = (b'a' + (i % 26) as u8) as char;
    if i < 26 { letter.to_string() } else { format!("{}{}", letter, i / 26) }
}

/// Collect free type-vars and row-vars in first-appearance order.
fn collect_pretty_vars(ty: &Ty, tvs: &mut Vec<TyVar>, rvs: &mut Vec<TyVar>) {
    match ty {
        Ty::Var(v) => { if !tvs.contains(v) { tvs.push(*v); } }
        Ty::List(t) => collect_pretty_vars(t, tvs, rvs),
        Ty::Func(a, b) => { collect_pretty_vars(a, tvs, rvs); collect_pretty_vars(b, tvs, rvs); }
        Ty::Record(row) => {
            for (_, t) in &row.fields { collect_pretty_vars(t, tvs, rvs); }
            if let RowTail::Open(v) = row.tail { if !rvs.contains(&v) { rvs.push(v); } }
        }
        Ty::Con(_, args) => { for a in args { collect_pretty_vars(a, tvs, rvs); } }
        _ => {}
    }
}

/// Render a `Ty` using caller-supplied name tables for type-vars and row-vars.
fn fmt_named(ty: &Ty, tvs: &[TyVar], rvs: &[TyVar], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match ty {
        Ty::Num  => write!(f, "Num"),
        Ty::Text => write!(f, "Text"),
        Ty::Bool => write!(f, "Bool"),
        Ty::List(t) => { write!(f, "List ")?; fmt_named_atomic(t, tvs, rvs, f) }
        Ty::Func(a, b) => {
            if matches!(a.as_ref(), Ty::Func(..)) {
                write!(f, "(")?; fmt_named(a, tvs, rvs, f)?; write!(f, ")")?;
            } else {
                fmt_named(a, tvs, rvs, f)?;
            }
            write!(f, " -> ")?;
            fmt_named(b, tvs, rvs, f)
        }
        Ty::Record(row) => {
            write!(f, "{{ ")?;
            for (i, (name, ty)) in row.fields.iter().enumerate() {
                if i > 0 { write!(f, ", ")?; }
                write!(f, "{}: ", name)?;
                fmt_named(ty, tvs, rvs, f)?;
            }
            if let RowTail::Open(v) = row.tail {
                if !row.fields.is_empty() { write!(f, ", ")?; }
                let name = rvs.iter().position(|x| x == &v)
                    .map(|i| pretty_var_name(tvs.len() + i))
                    .unwrap_or_else(|| format!("?{}", v));
                write!(f, "..{}", name)?;
            }
            write!(f, " }}")
        }
        Ty::Con(name, args) if args.is_empty() => write!(f, "{}", name),
        Ty::Con(name, args) => {
            write!(f, "{}", name)?;
            for a in args { write!(f, " ")?; fmt_named_atomic(a, tvs, rvs, f)?; }
            Ok(())
        }
        Ty::Var(v) => {
            let name = tvs.iter().position(|x| x == v)
                .map(pretty_var_name)
                .unwrap_or_else(|| format!("?{}", v));
            write!(f, "{}", name)
        }
    }
}

fn fmt_named_atomic(ty: &Ty, tvs: &[TyVar], rvs: &[TyVar], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let needs_parens = matches!(ty, Ty::Func(..)) || matches!(ty, Ty::Con(_, args) if !args.is_empty());
    if needs_parens { write!(f, "(")?; fmt_named(ty, tvs, rvs, f)?; write!(f, ")") }
    else { fmt_named(ty, tvs, rvs, f) }
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
        // Use the quantified vars (in their declared order) as the name source,
        // so `∀ a b. List a -> b` is consistent.
        fmt_named(&self.ty, &self.vars, &self.row_vars, f)
    }
}
