//! Format Lume source code.
//!
//! Two modes of operation:
//! - `format_source`: Full AST-based reformatting (may reorder, but preserves
//!   semantic meaning). Best for initial formatting of generated code.
//! - `normalize_source`: Whitespace-only normalization that preserves comments,
//!   blank line intent, and manual formatting choices. Best for user code.
//!
//! The `lume fmt` CLI uses `normalize_source` by default to avoid destroying
//! comments and intentional formatting.

use crate::ast::*;
use crate::pretty::{
    concat, concat_all, group, hardline, join, line, nest, nil, render, space, text, wrap, Doc,
};

// ── Configuration ──────────────────────────────────────────────────────────────

/// Formatting configuration.
#[derive(Clone, Debug)]
pub struct FormatConfig {
    /// Target line width. Default: 80.
    pub width: usize,
    /// Indentation step. Default: 2.
    pub indent: usize,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            width: 80,
            indent: 2,
        }
    }
}

// ── Public API: source-preserving normalization ────────────────────────────────

/// Normalize whitespace in Lume source without losing comments or structure.
///
/// Rules applied:
/// 1. Trailing whitespace removed from all lines
/// 2. Consecutive blank lines collapsed to at most one
/// 3. Indentation normalized to `config.indent` spaces per nesting level
/// 4. Exactly one trailing newline at EOF
/// 5. Comments preserved exactly (content not touched)
pub fn normalize_source(src: &str, config: &FormatConfig) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = String::with_capacity(src.len());
    let mut blank_count: usize = 0;
    let ind = config.indent;

    // Track nesting depth from structural keywords.
    let mut depth: usize = 0;

    for raw_line in &lines {
        let trimmed = raw_line.trim();

        // Blank lines: allow at most one consecutive blank line.
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 1 {
                out.push('\n');
            }
            continue;
        }
        blank_count = 0;

        // Comments: preserve verbatim with current indentation.
        if trimmed.starts_with("--") {
            push_indented(&mut out, depth, ind, trimmed);
            out.push('\n');
            continue;
        }

        // Dedent before closing braces.
        if starts_with_closer(trimmed) && depth > 0 {
            depth -= 1;
        }
        // Variant bars at type-def level get one indent
        if trimmed.starts_with('|') && !trimmed.starts_with("|>") && !trimmed.starts_with("||") {
            push_indented(&mut out, depth + 1, ind, trimmed);
        } else {
            push_indented(&mut out, depth, ind, trimmed);
        }
        out.push('\n');

        // Adjust depth for next line based on this line's content.
        if opens_block(trimmed) {
            depth += 1;
        }
    }

    // Ensure exactly one trailing newline.
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Does this line open a new indentation block?
fn opens_block(line: &str) -> bool {
    if line.starts_with("type ") && line.ends_with('=') {
        return true;
    }
    if line.starts_with("trait ") && line.ends_with('{') {
        return true;
    }
    if line.starts_with("use ") && line.ends_with('{') {
        return true;
    }
    if line == "pub {" {
        return true;
    }
    false
}

/// Does this line start with a block-closer?
fn starts_with_closer(line: &str) -> bool {
    line.starts_with('}')
}

fn push_indented(out: &mut String, depth: usize, ind: usize, content: &str) {
    for _ in 0..(depth * ind) {
        out.push(' ');
    }
    out.push_str(content);
}

// ── Public API: full AST-based formatting ──────────────────────────────────────

/// Parse and format source text with full AST reformatting.
/// Comments are preserved by re-attaching them based on source line positions.
/// Returns `None` if parsing fails.
pub fn format_source(src: &str, config: &FormatConfig) -> Option<String> {
    let lexer = crate::lexer::Lexer::new(src);
    let (tokens, comments) = lexer.tokenize_with_comments().ok()?;
    let mut program = crate::parser::parse_program(&tokens).ok()?;
    program.pragmas = crate::loader::parse_pragmas(src).0;

    // Build the AST-formatted output
    let ast_output = format_program(&program, config);

    if comments.is_empty() {
        return Some(ast_output);
    }

    // Strategy: walk through source lines and AST output lines together.
    // Source lines tagged as CODE get replaced by AST output lines.
    // Source lines tagged as COMMENT are preserved.
    // Blank lines are collapsed to at most one.
    let src_lines: Vec<&str> = src.lines().collect();

    // Build a map: source_line (1-based) → comment text
    let mut comment_lines: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut comment_texts: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();
    for c in &comments {
        comment_lines.insert(c.line);
        let formatted = if c.text.is_empty() {
            "--".to_string()
        } else {
            format!("-- {}", c.text)
        };
        comment_texts.insert(c.line, formatted);
    }

    // Collect AST output lines (non-blank code lines in order)
    let ast_lines: Vec<&str> = ast_output.lines().collect();
    let mut ast_code_lines: Vec<&str> = Vec::new();
    let mut ast_blanks_before: Vec<usize> = Vec::new(); // number of blank lines before this code line
    let mut blanks = 0usize;
    for line in &ast_lines {
        if line.trim().is_empty() {
            blanks += 1;
        } else {
            ast_blanks_before.push(blanks);
            ast_code_lines.push(line);
            blanks = 0;
        }
    }

    // Collect source code lines (non-blank, non-comment) in order
    let mut src_code_indices: Vec<usize> = Vec::new(); // 0-based line indices
    for (i, line) in src_lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let line_num = i + 1; // 1-based
        if comment_lines.contains(&line_num) {
            continue;
        }
        // Skip doc comments (--- ) — they're in the AST
        if trimmed.starts_with("---") {
            continue;
        }
        src_code_indices.push(i);
    }

    // Build mapping: src_code_idx[i] → ast_code_lines[i]
    // They should be 1:1 (same number of code lines)
    let mut result = String::with_capacity(src.len() + 256);
    let mut ast_idx = 0;
    let mut src_blank_count: usize = 0;

    for (i, line) in src_lines.iter().enumerate() {
        let trimmed = line.trim();
        let line_num = i + 1;

        // Blank line — track count (will use max of source and AST blanks)
        if trimmed.is_empty() {
            src_blank_count += 1;
            continue;
        }

        // Comment line
        if comment_lines.contains(&line_num) {
            // Emit blank line before comment if source had one
            if src_blank_count > 0 {
                result.push('\n');
            }
            src_blank_count = 0;
            if let Some(text) = comment_texts.get(&line_num) {
                let indent = if ast_idx < ast_code_lines.len() {
                    let ast_line = ast_code_lines[ast_idx];
                    &ast_line[..ast_line.len() - ast_line.trim_start().len()]
                } else {
                    let s = *line;
                    &s[..s.len() - s.trim_start().len()]
                };
                result.push_str(indent);
                result.push_str(text);
                result.push('\n');
            }
            continue;
        }

        // Doc comment line (part of AST — skip, emitted by AST formatter)
        if trimmed.starts_with("---") {
            continue;
        }

        // Code line — emit corresponding AST output
        if ast_idx < ast_code_lines.len() {
            // Determine blank lines: max of (source blanks capped at 1, AST blanks)
            let source_blanks = src_blank_count.min(1);
            let ast_blanks = if ast_idx > 0 { ast_blanks_before[ast_idx] } else { 0 };
            let wanted = source_blanks.max(ast_blanks);

            // Count how many trailing newlines already in result
            let trailing = result.as_bytes().iter().rev().take_while(|&&b| b == b'\n').count();
            let have_blanks = if trailing > 1 { trailing - 1 } else { 0 };
            let extra = wanted.saturating_sub(have_blanks);
            for _ in 0..extra {
                result.push('\n');
            }

            result.push_str(ast_code_lines[ast_idx]);
            result.push('\n');
            ast_idx += 1;
        }
        src_blank_count = 0;
    }

    // Emit any remaining AST lines
    while ast_idx < ast_code_lines.len() {
        let ast_blanks = if ast_idx > 0 { ast_blanks_before[ast_idx] } else { 0 };
        let trailing = result.as_bytes().iter().rev().take_while(|&&b| b == b'\n').count();
        let have_blanks = if trailing > 1 { trailing - 1 } else { 0 };
        let extra = ast_blanks.saturating_sub(have_blanks);
        for _ in 0..extra {
            result.push('\n');
        }
        result.push_str(ast_code_lines[ast_idx]);
        result.push('\n');
        ast_idx += 1;
    }

    // Ensure trailing newline (but don't strip intentional blanks)
    if !result.ends_with('\n') {
        result.push('\n');
    }

    Some(result)
}

/// Format a parsed program with the given configuration.
pub fn format_program(program: &Program, config: &FormatConfig) -> String {
    let doc = fmt_program(program, config.indent);
    let mut out = render(doc, config.width);
    let trimmed_len = out.trim_end().len();
    out.truncate(trimmed_len);
    out.push('\n');
    out
}

// ── Program ────────────────────────────────────────────────────────────────────

fn fmt_program(program: &Program, ind: usize) -> Doc {
    let mut parts: Vec<Doc> = Vec::new();

    if program.pragmas.internal {
        parts.push(text("-- lume internal"));
        parts.push(hardline());
    }
    if program.pragmas.no_prelude {
        parts.push(text("-- lume no-prelude"));
        parts.push(hardline());
    }
    if (program.pragmas.internal || program.pragmas.no_prelude)
        && (!program.uses.is_empty() || !program.items.is_empty())
    {
        parts.push(hardline());
    }

    for u in &program.uses {
        parts.push(fmt_use(u));
        parts.push(hardline());
    }
    if !program.uses.is_empty() && !program.items.is_empty() {
        parts.push(hardline());
        parts.push(hardline());
    }

    for (i, item) in program.items.iter().enumerate() {
        if i > 0 {
            parts.push(hardline());
        }
        parts.push(fmt_top_item(item, ind));
    }

    if let ExprKind::Record { entries, .. } = &program.exports.kind {
        if !entries.is_empty() {
            if !program.items.is_empty() || !program.uses.is_empty() {
                parts.push(hardline());
                parts.push(hardline());
                parts.push(hardline());
            }
            parts.push(fmt_pub_exports(entries, ind));
        }
    }

    concat_all(parts)
}

fn fmt_use(u: &UseDecl) -> Doc {
    let binding = match &u.binding {
        UseBinding::Ident(name, _, _) => text(name.clone()),
        UseBinding::Record(rp) => fmt_record_pattern(rp),
    };
    concat_all(vec![
        text("use "),
        binding,
        text(" = \""),
        text(u.path.clone()),
        text("\""),
    ])
}

fn fmt_pub_exports(entries: &[RecordEntry], ind: usize) -> Doc {
    let items: Vec<Doc> = entries
        .iter()
        .map(|entry| match entry {
            RecordEntry::Field(f) => match &f.value {
                None => text(f.name.clone()),
                Some(v) => concat_all(vec![
                    text(f.name.clone()),
                    text(": "),
                    fmt_expr(v, 0),
                ]),
            },
            RecordEntry::Spread(e) => concat(text(".."), fmt_expr(e, 0)),
        })
        .collect();
    let body = join(concat(text(","), hardline()), items);
    concat_all(vec![
        text("pub {"),
        nest(ind, concat(hardline(), body)),
        text(","),
        hardline(),
        text("}"),
    ])
}

// ── Top-level items ────────────────────────────────────────────────────────────

fn fmt_top_item(item: &TopItem, ind: usize) -> Doc {
    match item {
        TopItem::TypeDef(td) => fmt_typedef(td, ind),
        TopItem::Binding(b) => fmt_binding(b, ind),
        TopItem::BindingGroup(bs) => {
            let docs: Vec<Doc> = bs
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if i == 0 {
                        fmt_binding(b, ind)
                    } else {
                        concat(text("and "), fmt_binding_body(b, ind))
                    }
                })
                .collect();
            join(hardline(), docs)
        }
        TopItem::TraitDef(td) => fmt_trait_def(td, ind),
        TopItem::ImplDef(id) => fmt_impl_def(id, ind),
    }
}

// ── Type definitions ───────────────────────────────────────────────────────────

fn fmt_typedef(td: &TypeDef, ind: usize) -> Doc {
    let mut doc_parts = Vec::new();
    if let Some(doc) = &td.doc {
        for l in doc.lines() {
            doc_parts.push(text(format!("--- {}", l.trim())));
            doc_parts.push(hardline());
        }
    }

    let header = if td.params.is_empty() {
        format!("type {} =", td.name)
    } else {
        format!("type {} {} =", td.name, td.params.join(" "))
    };

    let variants: Vec<Doc> = td.variants.iter().map(fmt_variant).collect();
    let body = nest(ind, concat(hardline(), join(hardline(), variants)));
    let typedef_doc = concat(text(header), body);

    if doc_parts.is_empty() {
        typedef_doc
    } else {
        doc_parts.push(typedef_doc);
        concat_all(doc_parts)
    }
}

fn fmt_variant(v: &Variant) -> Doc {
    match &v.wraps {
        None => text(format!("| {}", v.name)),
        Some(ty) => concat_all(vec![text(format!("| {} ", v.name)), fmt_type(ty)]),
    }
}

// ── Trait definitions ──────────────────────────────────────────────────────────

fn fmt_trait_def(td: &TraitDef, ind: usize) -> Doc {
    let mut parts = Vec::new();
    if let Some(doc) = &td.doc {
        for l in doc.lines() {
            parts.push(text(format!("--- {}", l.trim())));
            parts.push(hardline());
        }
    }

    let header = text(format!("trait {} {} {{", td.name, td.type_param));
    let methods: Vec<Doc> = td
        .methods
        .iter()
        .map(|m| {
            let mut method_parts = Vec::new();
            if let Some(doc) = &m.doc {
                for l in doc.lines() {
                    method_parts.push(text(format!("--- {}", l.trim())));
                    method_parts.push(hardline());
                }
            }
            method_parts.push(concat_all(vec![
                text("let "),
                text(m.name.clone()),
                text(" : "),
                fmt_type(&m.ty),
            ]));
            concat_all(method_parts)
        })
        .collect();
    let body = nest(ind, concat(hardline(), join(hardline(), methods)));
    parts.push(concat_all(vec![header, body, hardline(), text("}")]));
    concat_all(parts)
}

// ── Impl definitions ───────────────────────────────────────────────────────────

fn fmt_impl_def(id: &ImplDef, ind: usize) -> Doc {
    let mut parts = Vec::new();
    if let Some(doc) = &id.doc {
        for l in doc.lines() {
            parts.push(text(format!("--- {}", l.trim())));
            parts.push(hardline());
        }
    }

    let mut header_parts = vec![
        text("use "),
        text(id.trait_name.clone()),
        text(" in "),
        text(id.type_name.clone()),
    ];
    if !id.impl_constraints.is_empty() {
        header_parts.push(text(" where "));
        let constraints: Vec<Doc> = id
            .impl_constraints
            .iter()
            .map(|(t, p)| text(format!("{} {}", t, p)))
            .collect();
        header_parts.push(join(text(", "), constraints));
    }
    header_parts.push(text(" {"));

    let methods: Vec<Doc> = id.methods.iter().map(|b| fmt_binding(b, ind)).collect();
    let body = nest(
        ind,
        concat(hardline(), join(concat(hardline(), hardline()), methods)),
    );
    parts.push(concat_all(vec![
        concat_all(header_parts),
        body,
        hardline(),
        text("}"),
    ]));
    concat_all(parts)
}

// ── Bindings ───────────────────────────────────────────────────────────────────

fn fmt_binding(b: &Binding, ind: usize) -> Doc {
    let mut parts = Vec::new();
    if let Some(doc) = &b.doc {
        for l in doc.lines() {
            parts.push(text(format!("--- {}", l.trim())));
            parts.push(hardline());
        }
    }
    parts.push(fmt_binding_body(b, ind));
    concat_all(parts)
}

fn fmt_binding_body(b: &Binding, ind: usize) -> Doc {
    let pat = fmt_pattern(&b.pattern);

    let constraints_doc = if b.constraints.is_empty() {
        nil()
    } else {
        let cs: Vec<Doc> = b
            .constraints
            .iter()
            .map(|(t, p)| text(format!("{} {}", t, p)))
            .collect();
        concat_all(vec![text("("), join(text(", "), cs), text(") => ")])
    };

    let ty_ann = match &b.ty {
        None => nil(),
        Some(ty) => concat_all(vec![text(" : "), constraints_doc, fmt_type(ty)]),
    };

    let rhs = fmt_expr_binding_rhs(&b.value, ind);
    concat_all(vec![text("let "), pat, ty_ann, text(" ="), rhs])
}

fn fmt_expr_binding_rhs(expr: &Expr, ind: usize) -> Doc {
    // Multi-arm top-level match: indent arms under the `=`
    if let ExprKind::Match(arms) = &expr.kind {
        if arms.len() > 1 {
            let arms_doc: Vec<Doc> = arms.iter().map(fmt_match_arm).collect();
            return nest(ind, concat(hardline(), concat_all(arms_doc)));
        }
    }

    // match ... in expression: single indent level
    if let ExprKind::MatchExpr { .. } = &expr.kind {
        let val = fmt_expr(expr, 0);
        return concat(text(" "), val);
    }

    // Lambda(s) ending in a multi-arm match: peel params, single indent level
    // e.g. `let scale = f -> | Circle ... | Rect ...`
    {
        let mut params: Vec<&Pattern> = Vec::new();
        let mut cur = expr;
        while let ExprKind::Lambda { param, body } = &cur.kind {
            params.push(param);
            cur = body;
        }
        if !params.is_empty() {
            let params_doc = join(
                text(" -> "),
                params.iter().map(|p| fmt_pattern(p)).collect::<Vec<_>>(),
            );

            // Lambda -> multi-arm match
            if let ExprKind::Match(arms) = &cur.kind {
                if arms.len() > 1 {
                    let arms_doc: Vec<Doc> = arms.iter().map(fmt_match_arm).collect();
                    return concat_all(vec![
                        text(" "),
                        params_doc,
                        text(" ->"),
                        nest(ind, concat(hardline(), concat_all(arms_doc))),
                    ]);
                }
            }
            // Lambda -> match expr
            if let ExprKind::MatchExpr { .. } = &cur.kind {
                let body_doc = fmt_expr(cur, 0);
                return concat_all(vec![
                    text(" "),
                    params_doc,
                    text(" -> "),
                    body_doc,
                ]);
            }
            // Lambda -> let-in chain
            if let ExprKind::LetIn { .. } = &cur.kind {
                let mut lets: Vec<(&Pattern, &Expr)> = Vec::new();
                while let ExprKind::LetIn { pattern, value, body } = &cur.kind {
                    lets.push((pattern, value));
                    cur = body;
                }
                let mut parts = Vec::new();
                for (pat, val) in &lets {
                    parts.push(text("let "));
                    parts.push(fmt_pattern(pat));
                    parts.push(text(" = "));
                    parts.push(fmt_expr(val, 0));
                    parts.push(text(" in"));
                    parts.push(hardline());
                }
                parts.push(fmt_expr(cur, 0));
                return concat_all(vec![
                    text(" "),
                    params_doc,
                    text(" ->"),
                    nest(ind, concat(hardline(), concat_all(parts))),
                ]);
            }
            // Lambda -> if-then-else (keep multiline)
            if let ExprKind::If { .. } = &cur.kind {
                let body_doc = fmt_expr(cur, 0);
                return concat_all(vec![
                    text(" "),
                    params_doc,
                    text(" ->"),
                    nest(ind, concat(hardline(), body_doc)),
                ]);
            }
        }
    }

    // Let-in chain: all lets and final body at same indent under `=`
    // e.g. `let aaa =\n  let x = 1 in\n  let y = 2 in\n  z`
    if let ExprKind::LetIn { .. } = &expr.kind {
        let mut lets: Vec<(&Pattern, &Expr)> = Vec::new();
        let mut cur = expr;
        while let ExprKind::LetIn { pattern, value, body } = &cur.kind {
            lets.push((pattern, value));
            cur = body;
        }
        let mut parts = Vec::new();
        for (pat, val) in &lets {
            parts.push(text("let "));
            parts.push(fmt_pattern(pat));
            parts.push(text(" = "));
            parts.push(fmt_expr(val, 0));
            parts.push(text(" in"));
            parts.push(hardline());
        }
        parts.push(fmt_expr(cur, 0));
        return nest(ind, concat(hardline(), concat_all(parts)));
    }

    // Default: simple expression
    let val = fmt_expr(expr, 0);
    group(nest(ind, concat(line(), val)))
}

// ── Expressions ────────────────────────────────────────────────────────────────

fn expr_prec(expr: &Expr) -> u8 {
    match &expr.kind {
        ExprKind::Lambda { .. } => 0,
        ExprKind::LetIn { .. } => 1,
        ExprKind::If { .. } => 1,
        ExprKind::Match(_) => 1,
        ExprKind::MatchExpr { .. } => 1,
        ExprKind::Binary { op, .. } => binop_prec(op),
        ExprKind::Unary { .. } => 70,
        ExprKind::Apply { .. } => 60,
        _ => 100,
    }
}

fn binop_prec(op: &BinOp) -> u8 {
    match op {
        BinOp::Pipe | BinOp::ResultPipe => 10,
        BinOp::Or => 20,
        BinOp::And => 30,
        BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => 40,
        BinOp::Concat | BinOp::Custom(_) => 45,
        BinOp::Add | BinOp::Sub => 50,
        BinOp::Mul | BinOp::Div => 55,
    }
}

fn binop_str(op: &BinOp) -> &str {
    match op {
        BinOp::Pipe => "|>",
        BinOp::ResultPipe => "?>",
        BinOp::Or => "||",
        BinOp::And => "&&",
        BinOp::Eq => "==",
        BinOp::NotEq => "!=",
        BinOp::Lt => "<",
        BinOp::Gt => ">",
        BinOp::LtEq => "<=",
        BinOp::GtEq => ">=",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Concat => "++",
        BinOp::Custom(s) => s.as_str(),
    }
}

fn fmt_expr(expr: &Expr, ctx_prec: u8) -> Doc {
    let my_prec = expr_prec(expr);
    let inner = fmt_expr_inner(expr);
    if my_prec < ctx_prec {
        wrap("(", ")", inner)
    } else {
        inner
    }
}

fn fmt_expr_inner(expr: &Expr) -> Doc {
    match &expr.kind {
        ExprKind::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                text(format!("{}", *n as i64))
            } else {
                text(format!("{}", n))
            }
        }
        ExprKind::Text(s) => text(format!("\"{}\"", escape_string(s))),
        ExprKind::Bool(b) => text(if *b { "true" } else { "false" }),
        ExprKind::Ident(name) => text(name.clone()),

        ExprKind::List { entries } => {
            if entries.is_empty() {
                return text("[]");
            }
            let items: Vec<Doc> = entries
                .iter()
                .map(|entry| match entry {
                    ListEntry::Elem(e) => fmt_expr(e, 0),
                    ListEntry::Spread(e) => concat(text(".."), fmt_expr(e, 0)),
                })
                .collect();
            let inner = join(concat(text(","), line()), items);
            group(concat_all(vec![
                text("["),
                nest(2, concat(line(), inner)),
                line(),
                text("]"),
            ]))
        }

        ExprKind::Record { entries } => fmt_record_expr(entries),

        ExprKind::FieldAccess { record, field } => {
            concat(fmt_expr(record, 100), text(format!(".{}", field)))
        }

        ExprKind::Variant { name, payload: None } => text(name.clone()),
        ExprKind::Variant { name, payload: Some(p) } => {
            concat(text(format!("{} ", name)), fmt_expr(p, 100))
        }

        ExprKind::Lambda { .. } => {
            let mut params: Vec<&Pattern> = Vec::new();
            let mut cur = expr;
            while let ExprKind::Lambda { param, body } = &cur.kind {
                params.push(param);
                cur = body;
            }
            let params_doc = join(
                text(" -> "),
                params.iter().map(|p| fmt_pattern(p)).collect::<Vec<_>>(),
            );
            if let ExprKind::Match(arms) = &cur.kind {
                if arms.len() > 1 {
                    let arms_doc: Vec<Doc> = arms.iter().map(fmt_match_arm).collect();
                    return concat_all(vec![
                        params_doc,
                        text(" ->"),
                        nest(2, concat(hardline(), concat_all(arms_doc))),
                    ]);
                }
            }
            concat(params_doc, concat(text(" -> "), fmt_expr(cur, 0)))
        }

        ExprKind::Apply { .. } => {
            let (func, args) = collect_apply(expr);
            let func_doc = fmt_expr(func, 60);
            let args_doc = join(
                line(),
                args.iter().map(|a| fmt_expr(a, 61)).collect::<Vec<_>>(),
            );
            group(concat(func_doc, nest(2, concat(line(), args_doc))))
        }

        ExprKind::Binary { op, left, right } => {
            let prec = binop_prec(op);
            let op_s = binop_str(op);
            // Left-associative: left operand at same prec needs no parens,
            // right operand at same prec gets parens.
            let l = fmt_expr(left, prec);
            let r = fmt_expr(right, prec + 1);
            group(concat_all(vec![l, line(), text(format!("{} ", op_s)), r]))
        }

        ExprKind::Unary { op, operand } => {
            let op_s = match op {
                UnOp::Neg => "-",
                UnOp::Not => "not ",
            };
            concat(text(op_s), fmt_expr(operand, 70))
        }

        ExprKind::If { cond, then_branch, else_branch } => {
            concat_all(vec![
                text("if "),
                fmt_expr(cond, 0),
                nest(2, concat(hardline(), concat(text("then "), fmt_expr(then_branch, 0)))),
                nest(2, concat(hardline(), concat(text("else "), fmt_expr(else_branch, 0)))),
            ])
        }

        ExprKind::Match(arms) => {
            let arms_doc: Vec<Doc> = arms.iter().map(fmt_match_arm).collect();
            concat_all(arms_doc)
        }

        ExprKind::MatchExpr { scrutinee, arms } => {
            let arms_doc: Vec<Doc> = arms.iter().map(fmt_match_arm).collect();
            concat_all(vec![
                text("match "),
                fmt_expr(scrutinee, 0),
                text(" in"),
                nest(2, concat(hardline(), concat_all(arms_doc))),
            ])
        }

        ExprKind::LetIn { .. } => {
            // Flatten let-in chains: successive lets and final body at same level.
            let mut lets: Vec<(&Pattern, &Expr)> = Vec::new();
            let mut cur = expr;
            while let ExprKind::LetIn { pattern, value, body } = &cur.kind {
                lets.push((pattern, value));
                cur = body;
            }
            let mut parts = Vec::new();
            for (pat, val) in &lets {
                parts.push(text("let "));
                parts.push(fmt_pattern(pat));
                parts.push(text(" = "));
                parts.push(fmt_expr(val, 0));
                parts.push(text(" in"));
                parts.push(hardline());
            }
            parts.push(fmt_expr(cur, 0));
            concat_all(parts)
        }

        ExprKind::TraitCall { trait_name, method_name } => {
            text(format!("{}.{}", trait_name, method_name))
        }

        ExprKind::Hole => text("_"),
    }
}

fn fmt_match_arm(arm: &MatchArm) -> Doc {
    let pat_doc = fmt_pattern(&arm.pattern);
    let guard_doc = match &arm.guard {
        None => nil(),
        Some(g) => concat(text(" if "), fmt_expr(g, 0)),
    };
    let body_doc = fmt_expr(&arm.body, 0);
    concat_all(vec![
        text("| "),
        pat_doc,
        guard_doc,
        text(" -> "),
        body_doc,
        hardline(),
    ])
}

fn collect_apply(expr: &Expr) -> (&Expr, Vec<&Expr>) {
    let mut args: Vec<&Expr> = Vec::new();
    let mut cur = expr;
    while let ExprKind::Apply { func, arg } = &cur.kind {
        args.push(arg);
        cur = func;
    }
    args.reverse();
    (cur, args)
}

fn escape_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

// ── Records ────────────────────────────────────────────────────────────────────

fn fmt_record_expr(entries: &[RecordEntry]) -> Doc {
    if entries.is_empty() {
        return text("{}");
    }
    let entry_docs: Vec<Doc> = entries
        .iter()
        .map(|entry| match entry {
            RecordEntry::Field(f) => {
                let val = match &f.value {
                    None => nil(),
                    Some(v) => concat(text(": "), fmt_expr(v, 0)),
                };
                concat(text(f.name.clone()), val)
            }
            RecordEntry::Spread(e) => concat(text(".."), fmt_expr(e, 0)),
        })
        .collect();
    let inner = join(concat(text(","), line()), entry_docs);
    group(wrap("{ ", " }", inner))
}

// ── Patterns ───────────────────────────────────────────────────────────────────

fn fmt_pattern(pat: &Pattern) -> Doc {
    match pat {
        Pattern::Wildcard => text("_"),
        Pattern::Literal(lit) => match lit {
            Literal::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    text(format!("{}", *n as i64))
                } else {
                    text(format!("{}", n))
                }
            }
            Literal::Text(s) => text(format!("\"{}\"", escape_string(s))),
            Literal::Bool(b) => text(if *b { "true" } else { "false" }),
        },
        Pattern::Ident(name, _, _) => text(name.clone()),
        Pattern::Variant { name, payload: None } => text(name.clone()),
        Pattern::Variant { name, payload: Some(p) } => {
            concat(text(format!("{} ", name)), fmt_pattern(p))
        }
        Pattern::Record(rp) => fmt_record_pattern(rp),
        Pattern::List(lp) => fmt_list_pattern(lp),
    }
}

fn fmt_record_pattern(rp: &RecordPattern) -> Doc {
    let mut parts: Vec<Doc> = rp
        .fields
        .iter()
        .map(|f| {
            let sub = match &f.pattern {
                None => nil(),
                Some(p) => concat(text(": "), fmt_pattern(p)),
            };
            concat(text(f.name.clone()), sub)
        })
        .collect();
    if let Some(rest) = &rp.rest {
        parts.push(match rest {
            None => text(".."),
            Some(name) => text(format!("..{}", name)),
        });
    }
    if parts.is_empty() {
        return text("{}");
    }
    let inner = join(text(", "), parts);
    concat_all(vec![text("{ "), inner, text(" }")])
}

fn fmt_list_pattern(lp: &ListPattern) -> Doc {
    let mut parts: Vec<Doc> = lp.elements.iter().map(fmt_pattern).collect();
    if let Some(rest) = &lp.rest {
        parts.push(match rest {
            None => text(".."),
            Some(name) => text(format!("..{}", name)),
        });
    }
    if parts.is_empty() {
        return text("[]");
    }
    let inner = join(text(", "), parts);
    concat_all(vec![text("["), inner, text("]")])
}

// ── Types ──────────────────────────────────────────────────────────────────────

fn fmt_type(ty: &Type) -> Doc {
    fmt_type_prec(ty, 0)
}

fn fmt_type_prec(ty: &Type, ctx: u8) -> Doc {
    match ty {
        Type::Var(name) => text(name.clone()),
        Type::Constructor(name) => text(name.clone()),
        Type::App { .. } => {
            let mut current: &Type = ty;
            let mut args = Vec::new();
            while let Type::App { callee, arg } = current {
                args.push(arg.as_ref());
                current = callee.as_ref();
            }
            args.reverse();
            let base_doc = fmt_type_prec(current, 10);
            let args_doc = join(
                space(),
                args.iter().map(|a| fmt_type_prec(a, 10)).collect::<Vec<_>>(),
            );
            let d = concat(concat(base_doc, space()), args_doc);
            if ctx >= 10 {
                wrap("(", ")", d)
            } else {
                d
            }
        }
        Type::Record(rt) => fmt_record_type(rt),
        Type::Func { param, ret } => {
            let d = concat_all(vec![
                fmt_type_prec(param, 1),
                text(" -> "),
                fmt_type_prec(ret, 0),
            ]);
            if ctx >= 1 {
                wrap("(", ")", d)
            } else {
                d
            }
        }
    }
}

fn fmt_record_type(rt: &RecordType) -> Doc {
    let mut parts: Vec<Doc> = rt
        .fields
        .iter()
        .map(|f| concat(text(format!("{}: ", f.name)), fmt_type(&f.ty)))
        .collect();
    if rt.open {
        parts.push(text(".."));
    }
    if parts.is_empty() {
        return text("{}");
    }
    let inner = join(text(", "), parts);
    concat_all(vec![text("{ "), inner, text(" }")])
}
