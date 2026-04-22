//! Fixity re-association pass.
//!
//! During parsing every `Token::Operator(s)` gets a single default binding-power
//! pair `(50, 50)`.  A fixity declaration (`infixl/infixr/infix N`) can assign
//! a *different* precedence.  Because fixity declarations may appear in any
//! module of a bundle (including imported ones), the parser cannot handle them
//! in a single-pass, single-file fashion.
//!
//! Instead we do a two-phase post-parse pass over the entire bundle:
//!
//! 1. [`collect_fixities`] — scan every module for operator bindings that
//!    carry a `Fixity` annotation and build a global [`FixityTable`].
//! 2. [`reassociate_bundle`] — walk every expression tree in every module and
//!    rebuild binary-operator sub-trees using the correct binding powers.
//!
//! ## Errors detected
//!
//! The pass also validates operator usage:
//!
//! - **Non-associative chaining** — an operator declared with `infix` (no
//!   associativity) appears in a chain with other operators at the same
//!   precedence level.  Example: `a =? b =? c` where `(=?)` is `infix 5`.
//!
//! - **Mixed associativity at the same precedence** — two operators at the
//!   same precedence level have conflicting associativity (one `infixl`, the
//!   other `infixr`).  Example: `a <+ b +> c` where `(<+)` is `infixl 5` and
//!   `(+>)` is `infixr 5`.  The parse result would be order-dependent, which
//!   is confusing and disallowed.

use std::collections::HashMap;

use crate::ast::{
    BinOp, DoStmt, Expr, ExprKind, FixityAssoc, ListEntry, MatchArm, Program, RecordEntry, TopItem,
};
use crate::bundle::BundleModule;
use crate::error::Span;

// ── Public types ──────────────────────────────────────────────────────────────

/// All information about a user-declared operator fixity.
#[derive(Clone, Debug)]
pub struct FixityEntry {
    /// Pre-computed Pratt binding-power pair.
    pub bps: (u8, u8),
    /// The original associativity declaration (needed for the non-assoc check).
    pub assoc: FixityAssoc,
}

/// Map from operator string (e.g. `"<>"`) to its fixity entry.
pub type FixityTable = HashMap<String, FixityEntry>;

// ── Phase 1: collect ──────────────────────────────────────────────────────────

/// Scan every module in the bundle and collect all operator fixity declarations
/// into a single table.
///
/// When two modules declare conflicting fixities for the same operator the *last*
/// one wins (bundle order is deterministic — dependencies come before dependants).
pub fn collect_fixities(modules: &[BundleModule]) -> FixityTable {
    let mut table = FixityTable::new();
    for m in modules {
        collect_from_program(&m.program, &mut table);
    }
    table
}

/// Collect operator fixity declarations from a single program into `table`.
///
/// Useful for the LSP, which analyses files individually rather than as a
/// full bundle.
pub fn collect_for_program(program: &Program) -> FixityTable {
    let mut table = FixityTable::new();
    collect_from_program(program, &mut table);
    table
}

fn collect_from_program(program: &Program, table: &mut FixityTable) {
    for item in &program.items {
        match item {
            TopItem::Binding(b) => {
                if let (Some(fixity), crate::ast::Pattern::Ident(name, ..)) =
                    (&b.fixity, &b.pattern)
                {
                    table.insert(
                        name.clone(),
                        FixityEntry { bps: fixity.to_binding_powers(), assoc: fixity.assoc.clone() },
                    );
                }
            }
            TopItem::BindingGroup(bindings) => {
                for b in bindings {
                    if let (Some(fixity), crate::ast::Pattern::Ident(name, ..)) =
                        (&b.fixity, &b.pattern)
                    {
                        table.insert(
                            name.clone(),
                            FixityEntry { bps: fixity.to_binding_powers(), assoc: fixity.assoc.clone() },
                        );
                    }
                }
            }
            TopItem::TraitDef(td) => {
                for method in &td.methods {
                    if let Some(ref fixity) = method.fixity {
                        table.insert(
                            method.name.clone(),
                            FixityEntry { bps: fixity.to_binding_powers(), assoc: fixity.assoc.clone() },
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

// ── Phase 2: re-association ───────────────────────────────────────────────────

/// Re-associate all binary-operator expression trees in every module of the
/// bundle using the global [`FixityTable`].
///
/// Returns `Err(message)` if any operator usage violates declared fixity
/// (non-associative chaining or mixed left/right associativity at the same
/// precedence level).
///
/// This is a no-op when `table` is empty (no user-defined fixities in the bundle).
pub fn reassociate_bundle(
    modules: &mut [BundleModule],
    table: &FixityTable,
) -> Result<(), String> {
    if table.is_empty() {
        return Ok(());
    }
    for m in modules {
        reassociate_program(&mut m.program, table)
            .map_err(|e| format!("{}: fixity error: {e}", m.canonical.display()))?;
    }
    Ok(())
}

fn reassociate_program(program: &mut Program, table: &FixityTable) -> Result<(), String> {
    for item in &mut program.items {
        match item {
            TopItem::Binding(b) => {
                b.value = reassociate_expr(
                    std::mem::replace(&mut b.value, placeholder()),
                    table,
                )?;
            }
            TopItem::BindingGroup(bindings) => {
                for b in bindings {
                    b.value = reassociate_expr(
                        std::mem::replace(&mut b.value, placeholder()),
                        table,
                    )?;
                }
            }
            TopItem::ImplDef(id) => {
                for method in &mut id.methods {
                    method.value = reassociate_expr(
                        std::mem::replace(&mut method.value, placeholder()),
                        table,
                    )?;
                }
            }
            _ => {}
        }
    }
    program.exports =
        reassociate_expr(std::mem::replace(&mut program.exports, placeholder()), table)?;
    Ok(())
}

// ── Core re-association algorithm ─────────────────────────────────────────────

/// An item in the flattened binary-operator spine.
enum FlatItem {
    Expr(Expr),
    Op(BinOp, Span),
}

/// Recursively re-associate `expr`, then flatten any top-level `Binary` spine
/// and rebuild it using the correct binding powers.
fn reassociate_expr(expr: Expr, table: &FixityTable) -> Result<Expr, String> {
    match expr.kind {
        ExprKind::Binary { .. } => {
            // Flatten the entire binary spine, recursing into non-binary sub-trees.
            let flat = flatten_and_recurse(expr, table)?;
            // Validate operator usage before rebuilding.
            check_flat_for_errors(&flat, table)?;
            let mut pos = 0;
            Ok(rebuild(&flat, &mut pos, table, 0))
        }
        _ => map_children_owned(expr, |child| reassociate_expr(child, table)),
    }
}

/// Flatten a contiguous binary-operator spine into a flat sequence of
/// alternating expressions and operators.  Non-binary sub-expressions are
/// recursed into (to fix their own internal binary structures) before being
/// placed in the flat list as opaque leaves.
fn flatten_and_recurse(expr: Expr, table: &FixityTable) -> Result<Vec<FlatItem>, String> {
    match expr.kind {
        ExprKind::Binary { op, left, right } => {
            let mut items = flatten_and_recurse(*left, table)?;
            items.push(FlatItem::Op(op, expr.span));
            items.extend(flatten_and_recurse(*right, table)?);
            Ok(items)
        }
        ExprKind::Paren(inner) => {
            let recursed = reassociate_expr(*inner, table)?;
            Ok(vec![FlatItem::Expr(Expr {
                id: expr.id,
                kind: ExprKind::Paren(Box::new(recursed)),
                span: expr.span,
            })])
        }
        _ => {
            let recursed = map_children_owned(expr, |child| reassociate_expr(child, table))?;
            Ok(vec![FlatItem::Expr(recursed)])
        }
    }
}

// ── Fixity validation ─────────────────────────────────────────────────────────

/// Validate operator usage in a flat binary spine.
///
/// Collects all operators grouped by precedence level (left bp) and checks:
///
/// 1. **Non-associative chaining** — any operator declared with `FixityAssoc::None`
///    that shares its precedence level with another operator in the expression.
///
/// 2. **Mixed associativity** — operators at the same precedence level where
///    some are left-associative (`r_bp > l_bp`) and others are right-associative
///    (`r_bp == l_bp`).  This is ambiguous because the rebuild result depends on
///    which operator appears first in the source text.
fn check_flat_for_errors(items: &[FlatItem], table: &FixityTable) -> Result<(), String> {
    // Group operators by l_bp (precedence level).
    let mut by_level: HashMap<u8, Vec<&BinOp>> = HashMap::new();
    for item in items {
        if let FlatItem::Op(op, _) = item {
            let (l_bp, _) = bp_for_op(op, table);
            by_level.entry(l_bp).or_default().push(op);
        }
    }

    for ops in by_level.values() {
        if ops.len() < 2 {
            continue;
        }

        // For each custom operator at this level, check the declared assoc.
        for op in ops.iter() {
            if let BinOp::Custom(name) = op {
                if let Some(entry) = table.get(name) {
                    if entry.assoc == FixityAssoc::None {
                        return Err(format!(
                            "operator '{}' is non-associative (declared with 'infix') \
                             and cannot be chained at the same precedence level; \
                             use parentheses to make the grouping explicit",
                            name
                        ));
                    }
                }
            }
        }

        // Check that all operators at this level have the same associativity
        // direction (left vs right).  r_bp == l_bp means right-assoc.
        let is_right: Vec<bool> = ops
            .iter()
            .map(|op| {
                let (l, r) = bp_for_op(op, table);
                r == l
            })
            .collect();

        let first_is_right = is_right[0];
        for (i, &right) in is_right.iter().enumerate().skip(1) {
            if right != first_is_right {
                let name1 = op_display(ops[0]);
                let name2 = op_display(ops[i]);
                let (assoc1, assoc2) = if first_is_right {
                    ("right-associative", "left-associative")
                } else {
                    ("left-associative", "right-associative")
                };
                return Err(format!(
                    "operators '{}' ({assoc1}) and '{}' ({assoc2}) are at the same \
                     precedence level but have conflicting associativity; \
                     mixing left- and right-associative operators at the same \
                     precedence is ambiguous — use parentheses or change precedence",
                    name1, name2
                ));
            }
        }
    }
    Ok(())
}

fn op_display(op: &BinOp) -> &str {
    match op {
        BinOp::Pipe => "|>",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Eq => "==",
        BinOp::NotEq => "!=",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::LtEq => "<=",
        BinOp::GtEq => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Concat => "++",
        BinOp::Custom(s) => s.as_str(),
    }
}

// ── Binding-power lookup ──────────────────────────────────────────────────────

/// Resolve `(left_bp, right_bp)` for a binary operator.
///
/// For `BinOp::Custom` operators the [`FixityTable`] is consulted first;
/// falling back to `(50, 50)` for unknown operators (right-assoc at the
/// `++` precedence level — same as the parser default).
/// Built-in operators use the same values as [`crate::parser`]'s `infix_bp`.
pub fn bp_for_op(op: &BinOp, table: &FixityTable) -> (u8, u8) {
    match op {
        BinOp::Pipe => (10, 11),
        BinOp::Or => (20, 21),
        BinOp::And => (30, 31),
        BinOp::Eq | BinOp::NotEq => (40, 41),
        BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => (42, 43),
        BinOp::Concat => (50, 50),
        BinOp::Add | BinOp::Sub => (60, 61),
        BinOp::Mul | BinOp::Div => (70, 71),
        BinOp::Custom(s) => table.get(s.as_str()).map(|e| e.bps).unwrap_or((50, 50)),
    }
}

// ── Pratt rebuild ─────────────────────────────────────────────────────────────

/// Rebuild a binary expression tree from a flat sequence using Pratt precedence.
fn rebuild(items: &[FlatItem], pos: &mut usize, table: &FixityTable, min_bp: u8) -> Expr {
    let mut lhs = match items.get(*pos) {
        Some(FlatItem::Expr(e)) => {
            let e = e.clone();
            *pos += 1;
            e
        }
        _ => panic!("fixity::rebuild: expected expression at pos {pos}"),
    };

    loop {
        if *pos >= items.len() {
            break;
        }
        let (op, op_span) = match &items[*pos] {
            FlatItem::Op(op, span) => (op.clone(), span.clone()),
            FlatItem::Expr(_) => break,
        };

        let (l_bp, r_bp) = bp_for_op(&op, table);
        if l_bp < min_bp {
            break;
        }
        *pos += 1;

        let rhs = rebuild(items, pos, table, r_bp);
        lhs = Expr {
            id: 0,
            kind: ExprKind::Binary {
                op,
                left: Box::new(lhs),
                right: Box::new(rhs),
            },
            span: op_span,
        };
    }
    lhs
}

// ── Fallible child-mapping utility ────────────────────────────────────────────

/// Apply `f` to every direct sub-expression of `expr` (consuming the original).
/// Returns `Err` if any child transform fails.
fn map_children_owned(
    expr: Expr,
    f: impl Fn(Expr) -> Result<Expr, String>,
) -> Result<Expr, String> {
    let Expr { id, kind, span } = expr;
    let kind = match kind {
        ExprKind::Number(_)
        | ExprKind::Text(_)
        | ExprKind::Bool(_)
        | ExprKind::Ident(_)
        | ExprKind::TraitCall { .. }
        | ExprKind::Hole => kind,

        ExprKind::Paren(inner) => ExprKind::Paren(Box::new(f(*inner)?)),

        ExprKind::List { entries } => ExprKind::List {
            entries: entries
                .into_iter()
                .map(|e| match e {
                    ListEntry::Elem(x) => f(x).map(ListEntry::Elem),
                    ListEntry::Spread(x) => f(x).map(ListEntry::Spread),
                })
                .collect::<Result<Vec<_>, _>>()?,
        },

        ExprKind::Record { entries } => ExprKind::Record {
            entries: entries
                .into_iter()
                .map(|e| match e {
                    RecordEntry::Field(mut rf) => {
                        if let Some(v) = rf.value.take() {
                            rf.value = Some(f(v)?);
                        }
                        Ok(RecordEntry::Field(rf))
                    }
                    RecordEntry::Spread(x) => f(x).map(RecordEntry::Spread),
                })
                .collect::<Result<Vec<_>, _>>()?,
        },

        ExprKind::FieldAccess { record, field } => ExprKind::FieldAccess {
            record: Box::new(f(*record)?),
            field,
        },

        ExprKind::Variant { name, payload } => ExprKind::Variant {
            name,
            payload: payload.map(|p| f(*p).map(Box::new)).transpose()?,
        },

        ExprKind::Lambda { param, body } => ExprKind::Lambda {
            param,
            body: Box::new(f(*body)?),
        },

        ExprKind::Apply { func, arg } => ExprKind::Apply {
            func: Box::new(f(*func)?),
            arg: Box::new(f(*arg)?),
        },

        ExprKind::Binary { op, left, right } => ExprKind::Binary {
            op,
            left: Box::new(f(*left)?),
            right: Box::new(f(*right)?),
        },

        ExprKind::Unary { op, operand } => ExprKind::Unary {
            op,
            operand: Box::new(f(*operand)?),
        },

        ExprKind::If { cond, then_branch, else_branch } => ExprKind::If {
            cond: Box::new(f(*cond)?),
            then_branch: Box::new(f(*then_branch)?),
            else_branch: Box::new(f(*else_branch)?),
        },

        ExprKind::Match(arms) => ExprKind::Match(try_map_arms(arms, &f)?),

        ExprKind::MatchExpr { scrutinee, arms } => ExprKind::MatchExpr {
            scrutinee: Box::new(f(*scrutinee)?),
            arms: try_map_arms(arms, &f)?,
        },

        ExprKind::LetIn { pattern, value, body } => ExprKind::LetIn {
            pattern,
            value: Box::new(f(*value)?),
            body: Box::new(f(*body)?),
        },

        ExprKind::Sequence(exprs) => ExprKind::Sequence(
            exprs.into_iter().map(f).collect::<Result<Vec<_>, _>>()?,
        ),

        ExprKind::Do { monad, stmts, tail } => ExprKind::Do {
            monad,
            stmts: stmts
                .into_iter()
                .map(|stmt| match stmt {
                    DoStmt::Let { pattern, value } => f(value).map(|v| DoStmt::Let { pattern, value: v }),
                    DoStmt::Bind { pattern, value } => f(value).map(|v| DoStmt::Bind { pattern, value: v }),
                    DoStmt::Seq(e) => f(e).map(DoStmt::Seq),
                })
                .collect::<Result<Vec<_>, _>>()?,
            tail: Box::new(f(*tail)?),
        },
    };
    Ok(Expr { id, kind, span })
}

fn try_map_arms(
    arms: Vec<MatchArm>,
    f: &impl Fn(Expr) -> Result<Expr, String>,
) -> Result<Vec<MatchArm>, String> {
    arms.into_iter()
        .map(|arm| {
            Ok(MatchArm {
                pattern: arm.pattern,
                guard: arm.guard.map(f).transpose()?,
                body: f(arm.body)?,
            })
        })
        .collect()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn placeholder() -> Expr {
    Expr {
        id: 0,
        kind: ExprKind::Hole,
        span: Span::default(),
    }
}
