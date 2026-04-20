//! Intermediate representation for Lume programs.
//!
//! Produced by the lowering pass (`lower.rs`) from the typed AST.
//! All trait constructs are erased: `TraitDef` and `ImplDef` become explicit
//! dictionary records, `TraitCall` becomes field access on the right dictionary,
//! and pipe operators are desugared to function application.

pub use crate::ast::{BinOp, UnOp};

// ── Module ───────────────────────────────────────────────────────────────────

/// A lowered module ready for code generation.
#[derive(Debug, Clone)]
pub struct Module {
    pub imports: Vec<Import>,
    pub items: Vec<Decl>,
    pub exports: Expr,
}

/// A resolved import: `use math = "./math"` or `use { area, pi } = "./math"`.
#[derive(Debug, Clone)]
pub struct Import {
    pub binding: ImportBinding,
    pub path: String,
}

#[derive(Debug, Clone)]
pub enum ImportBinding {
    /// `use math = "./math"` → binds module to a single name.
    Name(String),
    /// `use { area, pi } = "./math"` → destructures named fields.
    Destructure(Vec<String>),
}

// ── Declarations ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Decl {
    /// A single `let` binding.
    Let(Pat, Expr),
    /// Two or more mutually recursive bindings (`let … and …`).
    LetRec(Vec<(Pat, Expr)>),
}

// ── Expressions ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Expr {
    // Literals
    Num(f64),
    Str(String),
    Bool(bool),
    /// `[1, 2, 3]` or with spreads lowered to bases + trailing elems.
    List {
        /// Spread lists to concatenate, merged left-to-right.
        bases: Vec<Expr>,
        /// Trailing elements appended after all bases.
        elems: Vec<Expr>,
    },

    // Names
    Var(String),

    // Functions
    Lam(Pat, Box<Expr>),
    App(Box<Expr>, Box<Expr>),

    // Records
    Record {
        /// Base records to extend from, merged left-to-right.
        /// Each base contributes all its fields; later bases shadow earlier ones.
        bases: Vec<Expr>,
        /// Explicit fields applied on top of all bases.
        fields: Vec<(String, Expr)>,
    },
    Field(Box<Expr>, String),

    // Algebraic data types
    Tag(String, Option<Box<Expr>>),

    // Control flow
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Match(Box<Expr>, Vec<Branch>),
    /// Anonymous match (lambda-match): `| pat -> expr`
    MatchFn(Vec<Branch>),

    // Let expression
    Let(Pat, Box<Expr>, Box<Expr>),

    // Operators
    BinOp(BinOp, Box<Expr>, Box<Expr>),
    UnOp(UnOp, Box<Expr>),
}

// ── Branches ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Branch {
    pub pattern: Pat,
    pub guard: Option<Expr>,
    pub body: Expr,
}

// ── Patterns ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Pat {
    Wild,
    Var(String),
    Lit(Lit),
    Tag(String, Option<Box<Pat>>),
    Record {
        fields: Vec<(String, Option<Pat>)>,
        /// `None` = closed; `Some(None)` = `..`; `Some(Some(name))` = `..rest`
        rest: Option<Option<String>>,
    },
    List {
        elems: Vec<Pat>,
        /// `None` = closed; `Some(None)` = `..`; `Some(Some(name))` = `..rest`
        rest: Option<Option<String>>,
    },
}

#[derive(Debug, Clone)]
pub enum Lit {
    Num(f64),
    Str(String),
    Bool(bool),
}
