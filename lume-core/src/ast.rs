use crate::error::Span;

/// A unique identifier for every `Expr` node in the AST.
/// Assigned by `assign_node_ids` immediately after parsing.
pub type NodeId = u32;

/// Module-level pragmas declared via `-- lume <directive>` comment lines
/// at the top of a source file.
#[derive(Debug, Clone, Default)]
pub struct ModulePragmas {
    /// Module receives internal-only builtins (e.g. `list_map`).
    pub internal: bool,
    /// Module skips the automatic prelude import.
    pub no_prelude: bool,
    /// Module receives Map primitive builtins (only `lume:map` uses this).
    pub map_internal: bool,
}

/// The complete AST for a Lume source file.
///
/// ```text
/// program = use* (typedef | binding)* ("pub" expr)?
/// ```
#[derive(Debug, Clone)]
pub struct Program {
    pub uses: Vec<UseDecl>,
    pub items: Vec<TopItem>,
    /// The module's public interface. When `pub` is omitted, this is a
    /// synthetic empty record expression (`{}`).
    pub exports: Expr,
    /// Pragmas parsed from leading `-- lume …` comment lines.
    pub pragmas: ModulePragmas,
}

// ── Top-level items ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum TopItem {
    TypeDef(TypeDef),
    Binding(Binding),
    /// Two or more bindings joined by `and` that are mutually recursive.
    BindingGroup(Vec<Binding>),
    TraitDef(TraitDef),
    ImplDef(ImplDef),
}

// ── Operator fixity ───────────────────────────────────────────────────────────

/// Associativity for a user-defined infix operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixityAssoc {
    /// `infixl` — `a ⊕ b ⊕ c` parses as `(a ⊕ b) ⊕ c`.
    Left,
    /// `infixr` — `a ⊕ b ⊕ c` parses as `a ⊕ (b ⊕ c)`.
    Right,
    /// `infix` — non-associative; chaining at the same level is disallowed.
    /// Treated as left-assoc at parse time; enforced by a post-parse lint.
    None,
}

/// Fixity declaration on an operator binding or trait method.
///
/// ```text
/// let (++) infixr 6 = …        -- right-assoc at precedence 6
/// let (<>) infixl   = …        -- left-assoc at default precedence (9)
/// let (=?) infix  2 = …        -- non-assoc at precedence 2
/// ```
#[derive(Debug, Clone)]
pub struct Fixity {
    pub assoc: FixityAssoc,
    /// Precedence level 0–9.  Defaults to 9 when the number is omitted.
    pub prec: u8,
}

impl Fixity {
    /// Convert to the Pratt `(left_bp, right_bp)` pair used by the parser and
    /// the fixity re-association pass.
    ///
    /// Precedence *N* maps to binding-power `N × 8`, spreading user-defined
    /// operators evenly across the full built-in bp range:
    ///
    /// | Prec | bp (infixl/infix) | bp (infixr) | vs built-ins |
    /// |------|--------------------|-------------|--------------|
    /// | 0    | (0, 1)             | (0, 0)      | below `\|>` (10) |
    /// | 5    | (40, 41)           | (40, 40)    | same level as `==` (40) |
    /// | 7    | (56, 57)           | (56, 56)    | between `++` (50) and `+` (60) |
    /// | 8    | (64, 65)           | (64, 64)    | between `+` (60) and `*` (70) |
    /// | 9    | (72, 73)           | (72, 72)    | above `*` (70) |
    ///
    /// - `infixl N` → `(N*8, N*8+1)` — right recurses with higher bp → left-assoc
    /// - `infixr N` → `(N*8, N*8)`   — equal bps → right-assoc (Pratt convention)
    /// - `infix  N` → `(N*8, N*8+1)` — encoded like `infixl`; chaining detected
    ///   as a post-parse error by the fixity pass
    pub fn to_binding_powers(&self) -> (u8, u8) {
        let base = self.prec.saturating_mul(8);
        match self.assoc {
            FixityAssoc::Left | FixityAssoc::None => (base, base + 1),
            FixityAssoc::Right => (base, base),
        }
    }
}

/// `trait Show a { show: a -> Text }`
#[derive(Debug, Clone)]
pub struct TraitDef {
    pub name: String,
    pub type_param: String,
    pub methods: Vec<TraitMethod>,
    pub doc: Option<String>,
    /// Span of the trait name token (for hover).
    pub name_span: Span,
}

#[derive(Debug, Clone)]
pub struct TraitMethod {
    pub name: String,
    /// Span of the method name token (for hover on trait method declarations).
    pub name_span: Span,
    pub ty: Type,
    pub doc: Option<String>,
    /// Operator fixity, e.g. `let (++) infixr 6 : a -> a -> a`.
    pub fixity: Option<Fixity>,
}

/// `use Show in Num { show = x -> show x }`
#[derive(Debug, Clone)]
pub struct ImplDef {
    pub trait_name: String,
    /// Canonical string key for dict naming, e.g. `"Box Num"` or `"List a"`.
    pub type_name: String,
    /// Structured AST type for the impl target.  Used by the type checker to
    /// constrain method bodies against the trait's declared signatures.
    pub target_type: Type,
    /// Constraints on type variables: `where Show a` → `[("Show", "a")]`.
    /// Non-empty only for parameterized impls like `use Show in List a where Show a`.
    pub impl_constraints: Vec<(String, String)>,
    pub methods: Vec<Binding>,
    pub doc: Option<String>,
    /// Span of the trait name token in the impl header (for hover).
    pub trait_name_span: Span,
    /// Span of the type name in the impl header (for hover).
    pub type_name_span: Span,
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
    Ident(String, Span, NodeId),
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
    pub doc: Option<String>,
    /// Span of the type name token (for document symbols / go-to-definition).
    pub name_span: Span,
}

#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    /// The type this variant wraps (if any).
    /// Unit variants have `None`. Wrapper variants carry a type, which may
    /// be a plain type (`Some a`) or a record type (`Circle { radius: Num }`).
    pub wraps: Option<Type>,
    /// Span of the variant name token.
    pub name_span: Span,
}

/// `let x : (C a, C b) => T = expr`
#[derive(Debug, Clone)]
pub struct Binding {
    pub pattern: Pattern,
    /// Operator fixity, e.g. `let (++) infixr 6 = …`.  `None` for normal bindings.
    pub fixity: Option<Fixity>,
    /// Parsed constraint annotations: `(ToText a, ToText b) =>`.
    pub constraints: Vec<(String, String)>,
    pub ty: Option<Type>,
    pub value: Expr,
    pub doc: Option<String>,
}

// ── Expressions ───────────────────────────────────────────────────────────────

/// An expression together with its source location and a unique node ID.
#[derive(Debug, Clone)]
pub struct Expr {
    pub id: NodeId,
    pub kind: ExprKind,
    pub span: Span,
}

/// The expression payload - identical to the old `Expr` enum, but now children
/// are `Box<Expr>` (the wrapper) so every node in the tree carries a span.
#[derive(Debug, Clone)]
pub enum ExprKind {
    // Literals
    Number(f64),
    Text(String),
    Bool(bool),
    /// `[1, 2, 3]` or with spreads: `[..a, 4, ..b]`
    List {
        entries: Vec<ListEntry>,
    },

    // Names
    Ident(String),

    /// `{ name: "Alice", age: 30 }`
    /// Spreads and fields can be interleaved: `{ ..base, name: "Bob", ..extra }`
    /// Entries are applied left-to-right; later entries shadow earlier duplicates.
    Record {
        entries: Vec<RecordEntry>,
    },

    /// `alice.name`
    FieldAccess {
        record: Box<Expr>,
        field: String,
    },

    /// `Circle { radius: 5 }` or `North` (unit variant)
    Variant {
        name: String,
        payload: Option<Box<Expr>>,
    },

    /// `n -> n * 2`
    Lambda {
        param: Pattern,
        body: Box<Expr>,
    },

    /// `f x y`  represented as nested binary Apply nodes
    Apply {
        func: Box<Expr>,
        arg: Box<Expr>,
    },

    /// Binary operator expression
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },

    /// Parenthesized expression.
    /// Kept in the AST so passes like fixity reassociation can respect
    /// explicit grouping from the source.
    Paren(Box<Expr>),

    /// `not x`  or  unary `-x`
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },

    /// `if cond then a else b`
    If {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },

    /// `| pattern guard? -> expr` arms (anonymous match / lambda-match)
    Match(Vec<MatchArm>),

    /// `match expr in | pattern -> expr ...` (explicit scrutinee match)
    MatchExpr {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },

    /// `let pattern = value in body`
    LetIn {
        pattern: Pattern,
        value: Box<Expr>,
        body: Box<Expr>,
    },

    /// `Show.show` — a qualified trait method reference.
    /// Resolved by the type checker to a concrete dict field access.
    TraitCall {
        trait_name: String,
        method_name: String,
    },

    /// A typed hole `_` in expression position.
    /// The type checker infers the expected type and reports it as a diagnostic.
    Hole,
}

/// An entry in a record expression: either a named field or a spread.
#[derive(Debug, Clone)]
pub enum RecordEntry {
    Field(RecordField),
    Spread(Expr),
}

/// An entry in a list expression: either a single element or a spread.
#[derive(Debug, Clone)]
pub enum ListEntry {
    Elem(Expr),
    Spread(Expr),
}

#[derive(Debug, Clone)]
pub struct RecordField {
    pub name: String,
    /// Span of the field name token (for hover on record keys).
    pub name_span: Span,
    /// NodeId assigned to this field name (for hover type lookup).
    pub name_node_id: NodeId,
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
    Pipe, // |>

    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,

    // Comparison
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,

    // Boolean
    And,
    Or,

    // Concatenation
    Concat, // ++

    // User-defined operator (e.g. <>, >>=, <*>)
    Custom(String),
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
    Ident(String, Span, NodeId),
    /// `Circle { radius }` or `North` (unit)
    Variant {
        name: String,
        payload: Option<Box<Pattern>>,
    },
    /// `{ name, age, .. }`
    Record(RecordPattern),
    /// `[x, ..rest]`
    List(ListPattern),
}

#[derive(Debug, Clone)]
pub struct RecordPattern {
    pub fields: Vec<FieldPattern>,
    /// `None` = closed row; `Some(None)` = `..`; `Some(Some((name, span, id)))` = `..rest`
    pub rest: Option<Option<(String, Span, NodeId)>>,
}

#[derive(Debug, Clone)]
pub struct FieldPattern {
    pub name: String,
    /// Span of the field name token (for hover on destructured fields).
    pub span: Span,
    /// NodeId for this field binding (assigned by `assign_node_ids`).
    pub node_id: NodeId,
    /// `None` means shorthand: `{ age }` binds `age`
    pub pattern: Option<Pattern>,
}

#[derive(Debug, Clone)]
pub struct ListPattern {
    pub elements: Vec<Pattern>,
    /// `None` = closed; `Some(None)` = `..`; `Some(Some((name, span, id)))` = `..rest`
    pub rest: Option<Option<(String, Span, NodeId)>>,
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
    /// A bare type constructor name: `Num`, `Text`, `List`, `Maybe`, `Result`.
    Constructor(String),
    /// A type application: `List Num` → `App(Constructor("List"), Constructor("Num"))`.
    /// Applications are left-associative, so `Result Num Text` →
    /// `App(App(Constructor("Result"), Constructor("Num")), Constructor("Text"))`.
    App { callee: Box<Type>, arg: Box<Type> },
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

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Constructor(name) => write!(f, "{}", name),
            Type::App { callee, arg } => {
                // Parenthesise the callee if it is a function type.
                match callee.as_ref() {
                    Type::Func { .. } => write!(f, "({})", callee)?,
                    _ => write!(f, "{}", callee)?,
                }
                // Parenthesise the argument if it is itself an application or function.
                match arg.as_ref() {
                    Type::App { .. } | Type::Func { .. } => write!(f, " ({})", arg),
                    _ => write!(f, " {}", arg),
                }
            }
            Type::Var(v) => write!(f, "{}", v),
            Type::Record(rt) => {
                write!(f, "{{ ")?;
                for (i, field) in rt.fields.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}: {}", field.name, field.ty)?;
                }
                if rt.open { write!(f, ", ..")?; }
                write!(f, " }}")
            }
            Type::Func { param, ret } => {
                // Parenthesise function params for clarity.
                match param.as_ref() {
                    Type::Func { .. } => write!(f, "({}) -> {}", param, ret),
                    _ => write!(f, "{} -> {}", param, ret),
                }
            }
        }
    }
}

// ── Node ID assignment ────────────────────────────────────────────────────────

/// Walk every `Expr` in `program` in pre-order and assign a unique `NodeId`.
/// Call this once immediately after parsing.
pub fn assign_node_ids(program: &mut Program) {
    let mut counter: NodeId = 0;
    for u in &mut program.uses {
        match &mut u.binding {
            UseBinding::Ident(_, _, id) => {
                *id = counter;
                counter += 1;
            }
            UseBinding::Record(rp) => {
                for fp in &mut rp.fields {
                    fp.node_id = counter;
                    counter += 1;
                }
            }
        }
    }
    for item in &mut program.items {
        match item {
            TopItem::Binding(b) => {
                assign_ids_pattern(&mut b.pattern, &mut counter);
                assign_ids_expr(&mut b.value, &mut counter);
            }
            TopItem::BindingGroup(bs) => {
                for b in bs {
                    assign_ids_pattern(&mut b.pattern, &mut counter);
                    assign_ids_expr(&mut b.value, &mut counter);
                }
            }
            TopItem::TypeDef(_) | TopItem::TraitDef(_) => {}
            TopItem::ImplDef(id) => {
                for b in &mut id.methods {
                    assign_ids_pattern(&mut b.pattern, &mut counter);
                    assign_ids_expr(&mut b.value, &mut counter);
                }
            }
        }
    }
    assign_ids_expr(&mut program.exports, &mut counter);
}

fn assign_ids_pattern(pat: &mut Pattern, counter: &mut NodeId) {
    match pat {
        Pattern::Ident(_, _, id) => {
            *id = *counter;
            *counter += 1;
        }
        Pattern::Record(rp) => {
            for fp in &mut rp.fields {
                fp.node_id = *counter;
                *counter += 1;
                if let Some(p) = &mut fp.pattern {
                    assign_ids_pattern(p, counter);
                }
            }
            if let Some(Some((_, _, nid))) = rp.rest.as_mut() {
                *nid = *counter;
                *counter += 1;
            }
        }
        Pattern::Variant {
            payload: Some(p), ..
        } => assign_ids_pattern(p, counter),
        Pattern::List(lp) => {
            for p in &mut lp.elements {
                assign_ids_pattern(p, counter);
            }
            if let Some(Some((_, _, nid))) = lp.rest.as_mut() {
                *nid = *counter;
                *counter += 1;
            }
        }
        _ => {}
    }
}

fn assign_ids_expr(expr: &mut Expr, counter: &mut NodeId) {
    expr.id = *counter;
    *counter += 1;
    match &mut expr.kind {
        ExprKind::List { entries } => {
            for entry in entries {
                match entry {
                    ListEntry::Elem(e) | ListEntry::Spread(e) => assign_ids_expr(e, counter),
                }
            }
        }
        ExprKind::Record { entries } => {
            for entry in entries {
                match entry {
                    RecordEntry::Field(f) => {
                        f.name_node_id = *counter;
                        *counter += 1;
                        if let Some(v) = &mut f.value {
                            assign_ids_expr(v, counter);
                        }
                    }
                    RecordEntry::Spread(e) => {
                        assign_ids_expr(e, counter);
                    }
                }
            }
        }
        ExprKind::FieldAccess { record, .. } => {
            assign_ids_expr(record, counter);
        }
        ExprKind::Variant {
            payload: Some(p), ..
        } => {
            assign_ids_expr(p, counter);
        }
        ExprKind::Lambda { param, body } => {
            assign_ids_pattern(param, counter);
            assign_ids_expr(body, counter);
        }
        ExprKind::Apply { func, arg } => {
            assign_ids_expr(func, counter);
            assign_ids_expr(arg, counter);
        }
        ExprKind::Binary { left, right, .. } => {
            assign_ids_expr(left, counter);
            assign_ids_expr(right, counter);
        }
        ExprKind::Paren(inner) => {
            assign_ids_expr(inner, counter);
        }
        ExprKind::Unary { operand, .. } => {
            assign_ids_expr(operand, counter);
        }
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            assign_ids_expr(cond, counter);
            assign_ids_expr(then_branch, counter);
            assign_ids_expr(else_branch, counter);
        }
        ExprKind::Match(arms) => {
            for arm in arms {
                assign_ids_pattern(&mut arm.pattern, counter);
                if let Some(g) = &mut arm.guard {
                    assign_ids_expr(g, counter);
                }
                assign_ids_expr(&mut arm.body, counter);
            }
        }
        ExprKind::MatchExpr { scrutinee, arms } => {
            assign_ids_expr(scrutinee, counter);
            for arm in arms {
                assign_ids_pattern(&mut arm.pattern, counter);
                if let Some(g) = &mut arm.guard {
                    assign_ids_expr(g, counter);
                }
                assign_ids_expr(&mut arm.body, counter);
            }
        }
        ExprKind::LetIn {
            pattern,
            value,
            body,
        } => {
            assign_ids_pattern(pattern, counter);
            assign_ids_expr(value, counter);
            assign_ids_expr(body, counter);
        }
        // Leaves: Number, Text, Bool, Ident, Variant { payload: None }
        _ => {}
    }
}
