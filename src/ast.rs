use crate::error::Span;

/// The complete AST for a Lume source file.
///
/// ```
/// program = use* (typedef | binding)* record_expr
/// ```
#[derive(Debug, Clone)]
pub struct Program {
    pub uses: Vec<UseDecl>,
    pub items: Vec<TopItem>,
    /// The final record expression that is the module's public interface.
    pub exports: Expr,
}

// ── Top-level items ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum TopItem {
    TypeDef(TypeDef),
    Binding(Binding),
}

/// `use math = "./math"`  or  `use { area, pi } = "./math"`
#[derive(Debug, Clone)]
pub struct UseDecl {
    pub binding: UseBinding,
    pub path: String,
}

#[derive(Debug, Clone)]
pub enum UseBinding {
    /// `use math = "./math"`
    Ident(String),
    /// `use { area, pi } = "./math"`  (destructure)
    Record(RecordPattern),
}

/// `type Shape = | Circle { radius: Num } | Rect { w: Num, h: Num }`
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub name: String,
    /// Type parameters: `a`, `b`, etc.
    pub params: Vec<String>,
    pub variants: Vec<Variant>,
}

#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    /// Unit variants have no payload; others carry a record type.
    pub payload: Option<RecordType>,
}

/// `let x : T = expr`
#[derive(Debug, Clone)]
pub struct Binding {
    pub pattern: Pattern,
    pub ty: Option<Type>,
    pub value: Expr,
}

// ── Expressions ───────────────────────────────────────────────────────────────

/// An expression together with its source location.
#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

/// The expression payload — identical to the old `Expr` enum, but now children
/// are `Box<Expr>` (the wrapper) so every node in the tree carries a span.
#[derive(Debug, Clone)]
pub enum ExprKind {
    // Literals
    Number(f64),
    Text(String),
    Bool(bool),
    List(Vec<Expr>),

    // Names
    Ident(String),

    /// `{ name: "Alice", age: 30 }`
    /// For record update: `{ alice | age: 31 }` the base is Some(alice).
    Record {
        base: Option<Box<Expr>>,
        fields: Vec<RecordField>,
        #[allow(dead_code)]
        spread: bool, // ends with `..` (spread pattern for modules)
    },

    /// `alice.name`
    FieldAccess { record: Box<Expr>, field: String },

    /// `Circle { radius: 5 }` or `North` (unit variant)
    Variant { name: String, payload: Option<Box<Expr>> },

    /// `n -> n * 2`
    Lambda { param: Pattern, body: Box<Expr> },

    /// `f x y`  represented as nested binary Apply nodes
    Apply { func: Box<Expr>, arg: Box<Expr> },

    /// Binary operator expression
    Binary { op: BinOp, left: Box<Expr>, right: Box<Expr> },

    /// `not x`  or  unary `-x`
    Unary { op: UnOp, operand: Box<Expr> },

    /// `if cond then a else b`
    If { cond: Box<Expr>, then_branch: Box<Expr>, else_branch: Box<Expr> },

    /// `| pattern guard? -> expr` arms
    Match(Vec<MatchArm>),
}

#[derive(Debug, Clone)]
pub struct RecordField {
    pub name: String,
    /// `None` means field shorthand: `{ age }` == `{ age: age }`
    pub value: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
}

// ── Binary / unary operators ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    // Pipe
    Pipe,       // |>
    ResultPipe, // ?>

    // Arithmetic
    Add, Sub, Mul, Div,

    // Comparison
    Eq, NotEq, Lt, Gt, LtEq, GtEq,

    // Boolean
    And, Or,

    // Concatenation
    Concat, // ++
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnOp {
    Neg, // unary minus
    Not, // `not`
}

// ── Patterns ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Pattern {
    /// `_`
    Wildcard,
    /// Literal match: `42`, `"hello"`, `true`
    Literal(Literal),
    /// Bind to a name: `x`
    Ident(String),
    /// `Circle { radius }` or `North` (unit)
    Variant { name: String, payload: Option<Box<Pattern>> },
    /// `{ name, age, .. }`
    Record(RecordPattern),
    /// `[x, ..rest]`
    List(ListPattern),
}

#[derive(Debug, Clone)]
pub struct RecordPattern {
    pub fields: Vec<FieldPattern>,
    /// `None` = closed row; `Some(None)` = `..`; `Some(Some("rest"))` = `..rest`
    pub rest: Option<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct FieldPattern {
    pub name: String,
    /// `None` means shorthand: `{ age }` binds `age`
    pub pattern: Option<Pattern>,
}

#[derive(Debug, Clone)]
pub struct ListPattern {
    pub elements: Vec<Pattern>,
    /// `None` = closed; `Some(None)` = `..`; `Some(Some("rest"))` = `..rest`
    pub rest: Option<Option<String>>,
}

// ── Literals ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Literal {
    Number(f64),
    Text(String),
    Bool(bool),
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Type {
    /// `Num`, `Text`, `List a`, `Tree a`
    Named { name: String, args: Vec<Type> },
    /// Type variable: `a`, `b`, `r`
    Var(String),
    /// `{ name: Text, age: Num, .. }`
    Record(RecordType),
    /// `Text -> Num`
    Func { param: Box<Type>, ret: Box<Type> },
}

#[derive(Debug, Clone)]
pub struct RecordType {
    pub fields: Vec<FieldType>,
    /// Whether the row is open (`..`)
    pub open: bool,
}

#[derive(Debug, Clone)]
pub struct FieldType {
    pub name: String,
    pub ty: Type,
}
