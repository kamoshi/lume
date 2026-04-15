use crate::error::Span;

/// A unique identifier for every `Expr` node in the AST.
/// Assigned by `assign_node_ids` immediately after parsing.
pub type NodeId = u32;

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

/// `trait Show a { show: a -> Text }`
#[derive(Debug, Clone)]
pub struct TraitDef {
    pub name: String,
    pub type_param: String,
    pub methods: Vec<TraitMethod>,
}

#[derive(Debug, Clone)]
pub struct TraitMethod {
    pub name: String,
    pub ty: Type,
}

/// `use Show in Num { show = x -> show x }`
#[derive(Debug, Clone)]
pub struct ImplDef {
    pub trait_name: String,
    pub type_name: String,
    pub methods: Vec<Binding>,
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

    /// `| pattern guard? -> expr` arms
    Match(Vec<MatchArm>),

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
    Pipe,       // |>
    ResultPipe, // ?>

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
    /// `None` = closed row; `Some(None)` = `..`; `Some(Some("rest"))` = `..rest`
    pub rest: Option<Option<String>>,
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
            if let Some(Some(rest_name_pat)) = rp.rest.as_mut().map(|r| r.as_mut()) {
                // rest is just a String, no pattern node to assign
                let _ = rest_name_pat;
            }
        }
        Pattern::Variant {
            payload: Some(p), ..
        } => assign_ids_pattern(p, counter),
        Pattern::List(lp) => {
            for p in &mut lp.elements {
                assign_ids_pattern(p, counter);
            }
        }
        _ => {}
    }
}

fn assign_ids_expr(expr: &mut Expr, counter: &mut NodeId) {
    expr.id = *counter;
    *counter += 1;
    match &mut expr.kind {
        ExprKind::List(exprs) => {
            for e in exprs {
                assign_ids_expr(e, counter);
            }
        }
        ExprKind::Record { base, fields, .. } => {
            if let Some(b) = base {
                assign_ids_expr(b, counter);
            }
            for f in fields {
                f.name_node_id = *counter;
                *counter += 1;
                if let Some(v) = &mut f.value {
                    assign_ids_expr(v, counter);
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
