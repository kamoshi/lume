//! Functional recursive-descent parser with a Pratt sub-parser for binary
//! expressions.
//!
//! Every parsing function shares the same contract:
//!
//! ```text
//! fn parse_xxx(tokens: &[Spanned]) -> Result<(usize, T), ParseError>
//! ```
//!
//! - `tokens` is the remaining (unconsumed) token slice.
//! - On success the function returns `(consumed, node)`.
//! - The caller advances its local `ptr` by `consumed` and passes
//!   `&tokens[ptr..]` to subsequent calls.
//!
//! This makes backtracking trivial: if a speculative call fails, simply
//! don't advance `ptr`.

use crate::ast::{self, *};
use crate::error::{ParseError, Span};
use crate::lexer::{Spanned, Token};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return the span of `tokens[0]`, or a default span if the slice is empty.
fn span(tokens: &[Spanned]) -> Span {
    tokens.first().map(|t| t.span.clone()).unwrap_or_default()
}

/// Assert that `tokens[0].token == expected` and return `(1, ())`.
fn consume(tokens: &[Spanned], expected: &Token) -> Result<usize, ParseError> {
    match tokens.first() {
        Some(t) if &t.token == expected => Ok(1),
        Some(t) => Err(ParseError::unexpected(
            format!("{:?}", t.token),
            format!("{:?}", expected),
            t.span.clone(),
        )),
        None => Err(ParseError::unexpected_eof(Span::default())),
    }
}

/// Like `consume` but also returns the matched identifier string.
fn consume_ident(tokens: &[Spanned]) -> Result<(usize, String), ParseError> {
    match tokens.first() {
        Some(Spanned {
            token: Token::Ident(s),
            ..
        }) => Ok((1, s.clone())),
        Some(t) => Err(ParseError::unexpected(
            format!("{:?}", t.token),
            "identifier",
            t.span.clone(),
        )),
        None => Err(ParseError::unexpected_eof(Span::default())),
    }
}

/// Like `consume_ident` but also accepts a `TypeIdent` (uppercase-start).
/// Used for record field names which may be either case.
fn consume_any_ident(tokens: &[Spanned]) -> Result<(usize, String), ParseError> {
    match tokens.first() {
        Some(Spanned {
            token: Token::Ident(s),
            ..
        })
        | Some(Spanned {
            token: Token::TypeIdent(s),
            ..
        }) => Ok((1, s.clone())),
        Some(t) => Err(ParseError::unexpected(
            format!("{:?}", t.token),
            "identifier",
            t.span.clone(),
        )),
        None => Err(ParseError::unexpected_eof(Span::default())),
    }
}

/// Like `consume` but also returns the matched type identifier string.
fn consume_type_ident(tokens: &[Spanned]) -> Result<(usize, String), ParseError> {
    match tokens.first() {
        Some(Spanned {
            token: Token::TypeIdent(s),
            ..
        }) => Ok((1, s.clone())),
        Some(t) => Err(ParseError::unexpected(
            format!("{:?}", t.token),
            "type identifier",
            t.span.clone(),
        )),
        None => Err(ParseError::unexpected_eof(Span::default())),
    }
}

fn first_token(tokens: &[Spanned]) -> Option<&Token> {
    tokens.first().map(|t| &t.token)
}

/// Speculatively try to parse a constraint prefix: `(Trait a, Trait b) =>`
/// or unparenthesized: `Trait a =>` / `Trait a, Trait b =>`
/// Returns `Some((consumed, constraints))` on success, `None` on failure.
fn try_parse_constraints(tokens: &[Spanned]) -> Option<(usize, Vec<(String, String)>)> {
    // Try parenthesized form first: `(Trait a, Trait b) =>`
    if matches!(first_token(tokens), Some(Token::LParen)) {
        let mut ptr = 1; // skip `(`
        let mut constraints = Vec::new();
        #[allow(clippy::while_let_loop)]
        loop {
            // Expect TypeIdent (trait name) then Ident (type param)
            let trait_name = match tokens.get(ptr).map(|t| &t.token) {
                Some(Token::TypeIdent(s)) => s.clone(),
                _ => break,
            };
            ptr += 1;
            let param_name = match tokens.get(ptr).map(|t| &t.token) {
                Some(Token::Ident(s)) => s.clone(),
                _ => break,
            };
            ptr += 1;
            constraints.push((trait_name, param_name));
            // Expect `,` or `)`
            match tokens.get(ptr).map(|t| &t.token) {
                Some(Token::Comma) => {
                    ptr += 1; // skip `,` and continue
                }
                Some(Token::RParen) => {
                    ptr += 1; // skip `)`
                    // Must be followed by `=>`
                    if matches!(tokens.get(ptr).map(|t| &t.token), Some(Token::FatArrow)) {
                        ptr += 1; // skip `=>`
                        return Some((ptr, constraints));
                    }
                    break;
                }
                _ => break,
            }
        }
    }

    // Try unparenthesized form: `Trait a =>` or `Trait a, Trait b =>`
    // Look ahead for `=>` to avoid consuming non-constraint type expressions
    let fat_arrow_pos = tokens.iter().take(20).position(|t| matches!(t.token, Token::FatArrow))?;
    // Only try if `=>` is close enough to be constraints (at least 2 tokens per constraint)
    if fat_arrow_pos < 2 {
        return None;
    }
    let mut ptr = 0;
    let mut constraints = Vec::new();
    loop {
        let trait_name = match tokens.get(ptr).map(|t| &t.token) {
            Some(Token::TypeIdent(s)) => s.clone(),
            _ => return None,
        };
        ptr += 1;
        let param_name = match tokens.get(ptr).map(|t| &t.token) {
            Some(Token::Ident(s)) => s.clone(),
            _ => return None,
        };
        ptr += 1;
        constraints.push((trait_name, param_name));
        match tokens.get(ptr).map(|t| &t.token) {
            Some(Token::Comma) => ptr += 1,
            Some(Token::FatArrow) => {
                ptr += 1; // skip `=>`
                return Some((ptr, constraints));
            }
            _ => return None,
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Parse a complete Lume program.
///
/// ```text
/// program = use* (typedef | binding)* ("pub" expr)?
/// ```
/// Collect consecutive `DocComment` tokens at `tokens[ptr..]` and return the
/// number consumed and the merged doc string (or `None` if there are none).
fn collect_doc_comments(tokens: &[Spanned]) -> (usize, Option<String>) {
    let mut lines = Vec::new();
    let mut n = 0;
    while let Some(Token::DocComment(text)) = tokens.get(n).map(|t| &t.token) {
        lines.push(text.clone());
        n += 1;
    }
    if lines.is_empty() {
        (0, None)
    } else {
        (n, Some(lines.join("\n")))
    }
}

pub fn parse_program(tokens: &[Spanned]) -> Result<Program, ParseError> {
    let mut ptr = 0;

    // use declarations (module imports only; impl defs are deferred to items loop)
    let mut uses = Vec::new();
    while matches!(first_token(&tokens[ptr..]), Some(Token::Use)) {
        // `use TypeIdent in …` is an impl def — handled in items loop below.
        if matches!(
            tokens.get(ptr + 1).map(|t| &t.token),
            Some(Token::TypeIdent(_))
        ) {
            break;
        }
        let (n, u) = parse_use(&tokens[ptr..])?;
        ptr += n;
        uses.push(u);
    }

    // top-level type definitions, let bindings, trait defs, and impl defs
    let mut items = Vec::new();
    loop {
        // Collect doc comments preceding this item
        let (doc_n, pending_doc) = collect_doc_comments(&tokens[ptr..]);
        ptr += doc_n;

        match first_token(&tokens[ptr..]) {
            Some(Token::Type) => {
                let (n, mut td) = parse_typedef(&tokens[ptr..])?;
                ptr += n;
                td.doc = pending_doc;
                items.push(TopItem::TypeDef(td));
            }
            Some(Token::Let) => {
                let (n, mut b) = parse_binding(&tokens[ptr..])?;
                ptr += n;
                b.doc = pending_doc;
                // Collect `and let …` continuations into a mutually recursive group.
                if matches!(first_token(&tokens[ptr..]), Some(Token::And)) {
                    let mut group = vec![b];
                    while matches!(first_token(&tokens[ptr..]), Some(Token::And)) {
                        ptr += 1; // consume `and`
                        let (and_doc_n, and_doc) = collect_doc_comments(&tokens[ptr..]);
                        ptr += and_doc_n;
                        let (n, mut next) = parse_binding(&tokens[ptr..])?;
                        ptr += n;
                        next.doc = and_doc;
                        group.push(next);
                    }
                    items.push(TopItem::BindingGroup(group));
                } else {
                    items.push(TopItem::Binding(b));
                }
            }
            Some(Token::Trait) => {
                let (n, mut td) = parse_trait_def(&tokens[ptr..])?;
                ptr += n;
                td.doc = pending_doc;
                items.push(TopItem::TraitDef(td));
            }
            Some(Token::Use) => {
                // Must be an impl def (module imports consumed above).
                let (n, mut id) = parse_impl_def(&tokens[ptr..])?;
                ptr += n;
                id.doc = pending_doc;
                items.push(TopItem::ImplDef(id));
            }
            _ => break,
        }
    }

    // optional trailing `pub <expr>`
    let exports = if matches!(first_token(&tokens[ptr..]), Some(Token::Pub)) {
        ptr += 1;
        let (n, exports) = parse_expr(&tokens[ptr..])?;
        ptr += n;
        exports
    } else {
        Expr {
            id: 0,
            kind: ExprKind::Record {
                base: None,
                fields: vec![],
                spread: false,
            },
            span: span(&tokens[ptr..]),
        }
    };

    // expect EOF
    if !matches!(first_token(&tokens[ptr..]), Some(Token::Eof) | None) {
        return Err(ParseError::unexpected(
            format!("{:?}", tokens[ptr].token),
            "end of file",
            tokens[ptr].span.clone(),
        ));
    }

    let mut program = Program {
        uses,
        items,
        exports,
    };
    ast::assign_node_ids(&mut program);
    Ok(program)
}

// ── Use declarations ──────────────────────────────────────────────────────────

/// `use math = "./math"`  |  `use { area, pi } = "./math"`
fn parse_use(tokens: &[Spanned]) -> Result<(usize, UseDecl), ParseError> {
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::Use)?;

    let (n, binding) = match first_token(&tokens[ptr..]) {
        Some(Token::LBrace) => {
            let (n, rp) = parse_record_pattern(&tokens[ptr..])?;
            (n, UseBinding::Record(rp))
        }
        _ => {
            let ident_span = span(&tokens[ptr..]);
            let (n, name) = consume_ident(&tokens[ptr..])?;
            (n, UseBinding::Ident(name, ident_span, 0))
        }
    };
    ptr += n;
    ptr += consume(&tokens[ptr..], &Token::Equal)?;

    let path = match first_token(&tokens[ptr..]) {
        Some(Token::Text(s)) => s.clone(),
        Some(t) => {
            return Err(ParseError::unexpected(
                format!("{:?}", t),
                "string path",
                span(&tokens[ptr..]),
            ))
        }
        None => return Err(ParseError::unexpected_eof(span(&tokens[ptr..]))),
    };
    ptr += 1;

    Ok((ptr, UseDecl { binding, path }))
}

// ── Trait definitions ─────────────────────────────────────────────────────────

/// `trait Show a { show: a -> Text }`
fn parse_trait_def(tokens: &[Spanned]) -> Result<(usize, TraitDef), ParseError> {
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::Trait)?;

    let name_span = span(&tokens[ptr..]);
    let (n, name) = consume_type_ident(&tokens[ptr..])?;
    ptr += n;

    let (n, type_param) = consume_ident(&tokens[ptr..])?;
    ptr += n;

    ptr += consume(&tokens[ptr..], &Token::LBrace)?;

    let mut methods = Vec::new();
    while !matches!(first_token(&tokens[ptr..]), Some(Token::RBrace) | None) {
        let (method_doc_n, method_doc) = collect_doc_comments(&tokens[ptr..]);
        ptr += method_doc_n;
        ptr += consume(&tokens[ptr..], &Token::Let)?;
        let method_name_span = span(&tokens[ptr..]);
        let (n, method_name) = consume_ident(&tokens[ptr..])?;
        ptr += n;
        ptr += consume(&tokens[ptr..], &Token::Colon)?;
        let (n, ty) = parse_type(&tokens[ptr..])?;
        ptr += n;
        methods.push(TraitMethod { name: method_name, name_span: method_name_span, ty, doc: method_doc });
        // optional comma between methods
        if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
            ptr += 1;
        }
    }

    ptr += consume(&tokens[ptr..], &Token::RBrace)?;
    Ok((ptr, TraitDef { name, type_param, methods, doc: None, name_span }))
}

/// `use Show in Num { let show = x -> show x }`
/// `use Show in Show a => List a { let show = xs -> ... }`
fn parse_impl_def(tokens: &[Spanned]) -> Result<(usize, ImplDef), ParseError> {
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::Use)?;

    let trait_name_span = span(&tokens[ptr..]);
    let (n, trait_name) = consume_type_ident(&tokens[ptr..])?;
    ptr += n;

    ptr += consume(&tokens[ptr..], &Token::In)?;

    // Try to parse constraints before `=>`.
    // Constraints: `Show a, Eq a => ...`
    // If no `=>` appears, fall back to just the target type.
    let _type_name_start = ptr;
    let (impl_constraints, type_name, target_type, type_name_span) = {
        let saved = ptr;
        match try_parse_impl_constraints(&tokens[ptr..]) {
            Some((n, constraints)) => {
                ptr += n;
                let type_name_span = span(&tokens[ptr..]);
                let (n, type_name, target_type) = parse_impl_target_type(&tokens[ptr..])?;
                ptr += n;
                (constraints, type_name, target_type, type_name_span)
            }
            None => {
                ptr = saved;
                let type_name_span = span(&tokens[ptr..]);
                let (n, type_name, target_type) = parse_impl_target_type(&tokens[ptr..])?;
                ptr += n;
                (vec![], type_name, target_type, type_name_span)
            }
        }
    };

    ptr += consume(&tokens[ptr..], &Token::LBrace)?;

    let mut methods = Vec::new();
    while !matches!(first_token(&tokens[ptr..]), Some(Token::RBrace) | None) {
        let (method_doc_n, method_doc) = collect_doc_comments(&tokens[ptr..]);
        ptr += method_doc_n;
        ptr += consume(&tokens[ptr..], &Token::Let)?;
        let name_span = span(&tokens[ptr..]);
        let (n, method_name) = consume_ident(&tokens[ptr..])?;
        ptr += n;

        // optional type annotation
        let ty = if matches!(first_token(&tokens[ptr..]), Some(Token::Colon)) {
            ptr += 1; // consume `:`
            let (n, t) = parse_type(&tokens[ptr..])?;
            ptr += n;
            Some(t)
        } else {
            None
        };

        ptr += consume(&tokens[ptr..], &Token::Equal)?;
        let (n, value) = parse_expr(&tokens[ptr..])?;
        ptr += n;
        methods.push(Binding {
            pattern: Pattern::Ident(method_name, name_span, 0),
            constraints: vec![],
            ty,
            value,
            doc: method_doc,
        });
        // optional comma between methods
        if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
            ptr += 1;
        }
    }

    ptr += consume(&tokens[ptr..], &Token::RBrace)?;
    Ok((ptr, ImplDef { trait_name, type_name, target_type, impl_constraints, methods, doc: None, trait_name_span, type_name_span }))
}

/// Try to parse `Show a, Eq a =>` constraint list.  Returns `None` if `=>`
/// is not found (meaning this is a plain, unconstrained impl).
fn try_parse_impl_constraints(tokens: &[Spanned]) -> Option<(usize, Vec<(String, String)>)> {
    // Scan ahead to see if `=>` appears before `{`.
    let fat_arrow_pos = tokens.iter().position(|t| matches!(t.token, Token::FatArrow))?;
    // Make sure `{` doesn't appear before `=>`
    let lbrace_pos = tokens.iter().position(|t| matches!(t.token, Token::LBrace));
    if lbrace_pos.is_some_and(|p| p < fat_arrow_pos) {
        return None;
    }

    // Parse constraints: `TypeIdent ident [, TypeIdent ident]*`
    let mut ptr = 0;
    let mut constraints = Vec::new();
    loop {
        let trait_name = match first_token(&tokens[ptr..])? {
            Token::TypeIdent(s) => s.clone(),
            _ => return None,
        };
        ptr += 1;
        let type_var = match first_token(&tokens[ptr..])? {
            Token::Ident(s) => s.clone(),
            _ => return None,
        };
        ptr += 1;
        constraints.push((trait_name, type_var));

        match first_token(&tokens[ptr..])? {
            Token::Comma => ptr += 1,      // more constraints
            Token::FatArrow => {
                ptr += 1; // consume `=>`
                break;
            }
            _ => return None,
        }
    }
    Some((ptr, constraints))
}

/// Parse the target type in `use Trait in <target> { ... }`.
///
/// Accepts a type constructor optionally followed by type arguments.
/// Stops before `{` so that the opening brace of the impl body is not
/// consumed as a record type argument.
///
/// Examples: `Num`, `Box Num`, `Result Num Text`, `Box (List Num)`
fn parse_impl_target_type(tokens: &[Spanned]) -> Result<(usize, String, Type), ParseError> {
    // Accept a record type `{ field: Type, ... }` as an impl target.
    if matches!(first_token(tokens), Some(Token::LBrace)) {
        let (n, ty) = parse_type(tokens)?;
        let type_name = type_to_canonical_string(&ty);
        return Ok((n, type_name, ty));
    }

    let (n, head) = consume_type_ident(tokens)?;
    let mut ptr = n;
    let mut args: Vec<Type> = Vec::new();

    // Collect type arguments: TypeIdent, Ident (type var), or parenthesised.
    // LBrace is intentionally excluded so the impl body opener isn't consumed.
    loop {
        match first_token(&tokens[ptr..]) {
            Some(Token::TypeIdent(s)) => {
                args.push(Type::Named { name: s.clone(), args: vec![] });
                ptr += 1;
            }
            Some(Token::Ident(s)) => {
                args.push(Type::Var(s.clone()));
                ptr += 1;
            }
            Some(Token::LParen) => {
                ptr += 1; // consume `(`
                let (n, inner_ty) = parse_type(&tokens[ptr..])?;
                ptr += n;
                ptr += consume(&tokens[ptr..], &Token::RParen)?;
                args.push(inner_ty);
            }
            _ => break,
        }
    }

    let target_type = Type::Named { name: head, args };
    let type_name = type_to_canonical_string(&target_type);
    Ok((ptr, type_name, target_type))
}

/// Convert an AST `Type` to a canonical string for use in impl keys.
fn type_to_canonical_string(ty: &Type) -> String {
    match ty {
        Type::Named { name, args } if args.is_empty() => name.clone(),
        Type::Named { name, args } => {
            let arg_strs: Vec<String> = args.iter().map(|a| {
                let s = type_to_canonical_string(a);
                // Parenthesise applied types when they appear as arguments
                if matches!(a, Type::Named { args, .. } if !args.is_empty())
                    || matches!(a, Type::Func { .. })
                {
                    format!("({})", s)
                } else {
                    s
                }
            }).collect();
            format!("{} {}", name, arg_strs.join(" "))
        }
        Type::Var(v) => v.clone(),
        Type::Func { param, ret } => {
            format!("{} -> {}", type_to_canonical_string(param), type_to_canonical_string(ret))
        }
        Type::Record(rt) => {
            let mut sorted_fields: Vec<&FieldType> = rt.fields.iter().collect();
            sorted_fields.sort_by(|a, b| a.name.cmp(&b.name));
            let fields: Vec<String> = sorted_fields.iter().map(|f| {
                format!("{}: {}", f.name, type_to_canonical_string(&f.ty))
            }).collect();
            if rt.open {
                format!("{{ {}, .. }}", fields.join(", "))
            } else {
                format!("{{ {} }}", fields.join(", "))
            }
        }
    }
}

// ── Type definitions ──────────────────────────────────────────────────────────

/// `type Shape a = | Circle { radius: Num } | Rect { w: Num, h: Num }`
fn parse_typedef(tokens: &[Spanned]) -> Result<(usize, TypeDef), ParseError> {
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::Type)?;

    let (n, name) = consume_type_ident(&tokens[ptr..])?;
    ptr += n;

    // optional type parameters (lowercase identifiers)
    let mut params = Vec::new();
    while let Some(Token::Ident(p)) = first_token(&tokens[ptr..]) {
        params.push(p.clone());
        ptr += 1;
    }

    ptr += consume(&tokens[ptr..], &Token::Equal)?;

    // one or more variants: `| Variant payload?` or `Variant payload? (| Variant payload?)*`
    // The first `|` is optional.
    let mut variants = Vec::new();

    // Check if first variant starts with `|` (traditional) or directly with TypeIdent
    if matches!(first_token(&tokens[ptr..]), Some(Token::Bar)) {
        // Traditional: `| Variant ...`
        while matches!(first_token(&tokens[ptr..]), Some(Token::Bar)) {
            ptr += 1; // consume `|`
            let (n, v) = parse_variant(&tokens[ptr..])?;
            ptr += n;
            variants.push(v);
        }
    } else if matches!(first_token(&tokens[ptr..]), Some(Token::TypeIdent(_))) {
        // No leading pipe: `Variant ... | Variant ...`
        let (n, v) = parse_variant(&tokens[ptr..])?;
        ptr += n;
        variants.push(v);
        while matches!(first_token(&tokens[ptr..]), Some(Token::Bar)) {
            ptr += 1; // consume `|`
            let (n, v) = parse_variant(&tokens[ptr..])?;
            ptr += n;
            variants.push(v);
        }
    }

    if variants.is_empty() {
        return Err(ParseError {
            kind: crate::error::ParseErrorKind::EmptyTypeVariants,
            span: span(tokens),
        });
    }

    Ok((
        ptr,
        TypeDef {
            name,
            params,
            variants,
            doc: None,
        },
    ))
}

fn parse_variant(tokens: &[Spanned]) -> Result<(usize, Variant), ParseError> {
    let mut ptr = 0;
    let (n, name) = consume_type_ident(&tokens[ptr..])?;
    ptr += n;

    // record payload: `{ field: Type, ... }` — parsed as wrapping a record type
    if matches!(first_token(&tokens[ptr..]), Some(Token::LBrace)) {
        let (n, rt) = parse_record_type(&tokens[ptr..])?;
        ptr += n;
        let ty = Type::Record(rt);
        return Ok((ptr, Variant { name, wraps: Some(ty) }));
    }

    // single-value wrapper: `TestBox a`, `TestBox (List a)`
    // Must be a type expression that isn't the start of a new variant or the next `|`.
    if matches!(
        first_token(&tokens[ptr..]),
        Some(Token::Ident(_) | Token::TypeIdent(_) | Token::LParen)
    ) {
        // Only consume a wrapped type if it's not the start of a new variant
        // (i.e., a TypeIdent followed by `{` or `|` or next-line definition).
        // We try to parse a type; if it's a simple ident or paren type, take it.
        let (n, ty) = parse_type(&tokens[ptr..])?;
        ptr += n;
        return Ok((ptr, Variant { name, wraps: Some(ty) }));
    }

    Ok((ptr, Variant { name, wraps: None }))
}

// ── Let bindings ──────────────────────────────────────────────────────────────

/// `let pattern (: type)? = expr`
pub fn parse_binding(tokens: &[Spanned]) -> Result<(usize, Binding), ParseError> {
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::Let)?;

    let (n, pattern) = parse_pattern(&tokens[ptr..])?;
    ptr += n;

    // optional type annotation (possibly with constraints)
    let mut constraints: Vec<(String, String)> = Vec::new();
    let ty = if matches!(first_token(&tokens[ptr..]), Some(Token::Colon)) {
        ptr += 1; // consume `:`
        // Try to parse constraint prefix: `(Trait a, Trait b) =>`
        // We detect this by speculatively scanning for `( TypeIdent ident , ... ) =>`
        if let Some(parsed) = try_parse_constraints(&tokens[ptr..]) {
            ptr += parsed.0;
            constraints = parsed.1;
        }
        let (n, t) = parse_type(&tokens[ptr..])?;
        ptr += n;
        Some(t)
    } else {
        None
    };

    ptr += consume(&tokens[ptr..], &Token::Equal)?;

    let (n, value) = parse_expr(&tokens[ptr..])?;
    ptr += n;

    Ok((ptr, Binding { pattern, constraints, ty, value, doc: None }))
}

// ── Expressions ───────────────────────────────────────────────────────────────

/// Top-level expression parser.
///
/// ```text
/// expr = lambda | pipe_expr
/// ```
///
/// We try to parse a lambda first (pattern `->` body). If the next tokens
/// look like a pattern followed by `->` we commit to that branch.
/// Otherwise we fall through to the Pratt parser.
pub fn parse_expr(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    // `let pattern = value in body`
    if matches!(first_token(tokens), Some(Token::Let)) {
        if let Ok((n, expr)) = try_parse_let_in(tokens) {
            return Ok((n, expr));
        }
    }

    // Attempt lambda parse speculatively:
    //   - record destructure lambda:  `{ .. } ->`  or tuple: `(a, b) ->`
    //   - simple ident lambda:        `n ->`
    if let Ok((n, expr)) = try_parse_lambda(tokens) {
        return Ok((n, expr));
    }

    // fall through to Pratt for binary / pipe / apply expressions
    parse_pratt(tokens, 0)
}

/// Try to parse `let pattern (: type)? = value in body` without committing on failure.
fn try_parse_let_in(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    let let_span = span(tokens);
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::Let)?;

    let (n, pattern) = parse_pattern(&tokens[ptr..])?;
    ptr += n;

    // optional type annotation (ignored at runtime, but parsed)
    if matches!(first_token(&tokens[ptr..]), Some(Token::Colon)) {
        ptr += 1;
        let (n, _) = parse_type(&tokens[ptr..])?;
        ptr += n;
    }

    ptr += consume(&tokens[ptr..], &Token::Equal)?;

    let (n, value) = parse_expr(&tokens[ptr..])?;
    ptr += n;

    // Require `in` - if absent, this is a top-level binding, not a let-in expr.
    consume(&tokens[ptr..], &Token::In)?;
    ptr += 1;

    let (n, body) = parse_expr(&tokens[ptr..])?;
    ptr += n;

    Ok((
        ptr,
        Expr {
            id: 0,
            kind: ExprKind::LetIn {
                pattern,
                value: Box::new(value),
                body: Box::new(body),
            },
            span: let_span,
        },
    ))
}

/// Try to parse `pattern -> body` without committing on failure.
fn try_parse_lambda(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    let lambda_span = span(tokens);
    let mut ptr = 0;

    // Parse a pattern (may consume several tokens)
    let (n, param) = parse_pattern(&tokens[ptr..])?;
    ptr += n;

    // Must be followed immediately by `->`
    consume(&tokens[ptr..], &Token::Arrow)?;
    ptr += 1;

    let (n, body) = parse_expr(&tokens[ptr..])?;
    ptr += n;

    Ok((
        ptr,
        Expr {
            id: 0,
            kind: ExprKind::Lambda {
                param,
                body: Box::new(body),
            },
            span: lambda_span,
        },
    ))
}

// ── Pratt parser ──────────────────────────────────────────────────────────────

/// Binding powers for infix operators.
/// Returns `(left_bp, right_bp)` - right_bp > left_bp means right-associative.
fn infix_bp(tok: &Token) -> Option<(u8, u8)> {
    match tok {
        Token::Pipe => Some((10, 11)),       // |>  left-assoc
        Token::ResultPipe => Some((12, 13)), // ?>  left-assoc
        Token::PipePipe => Some((20, 21)),   // || left-assoc
        Token::AmpAmp => Some((30, 31)),     // && left-assoc
        Token::EqEq | Token::BangEq => Some((40, 41)),
        Token::Lt | Token::Gt | Token::LtEq | Token::GtEq => Some((40, 41)),
        Token::Concat => Some((50, 50)), // ++ right-assoc (equal bps)
        Token::Plus | Token::Minus => Some((60, 61)),
        Token::Star | Token::Slash => Some((70, 71)),
        _ => None,
    }
}

fn token_to_binop(tok: &Token) -> Option<BinOp> {
    match tok {
        Token::Pipe => Some(BinOp::Pipe),
        Token::ResultPipe => Some(BinOp::ResultPipe),
        Token::PipePipe => Some(BinOp::Or),
        Token::AmpAmp => Some(BinOp::And),
        Token::EqEq => Some(BinOp::Eq),
        Token::BangEq => Some(BinOp::NotEq),
        Token::Lt => Some(BinOp::Lt),
        Token::Gt => Some(BinOp::Gt),
        Token::LtEq => Some(BinOp::LtEq),
        Token::GtEq => Some(BinOp::GtEq),
        Token::Concat => Some(BinOp::Concat),
        Token::Plus => Some(BinOp::Add),
        Token::Minus => Some(BinOp::Sub),
        Token::Star => Some(BinOp::Mul),
        Token::Slash => Some(BinOp::Div),
        _ => None,
    }
}

/// Pratt (top-down operator precedence) parser.
/// `min_bp` is the minimum binding power the next infix operator must have to
/// be consumed by this call.
fn parse_pratt(tokens: &[Spanned], min_bp: u8) -> Result<(usize, Expr), ParseError> {
    let mut ptr = 0;

    // ── Prefix / primary ──────────────────────────────────────────────────────

    // Unary `not`
    if matches!(first_token(&tokens[ptr..]), Some(Token::Not)) {
        let unary_span = span(&tokens[ptr..]);
        ptr += 1;
        let (n, operand) = parse_pratt(&tokens[ptr..], 80)?;
        ptr += n;
        return Ok((
            ptr,
            Expr {
                id: 0,
                kind: ExprKind::Unary {
                    op: UnOp::Not,
                    operand: Box::new(operand),
                },
                span: unary_span,
            },
        ));
    }

    // Unary `-`
    if matches!(first_token(&tokens[ptr..]), Some(Token::Minus)) {
        let unary_span = span(&tokens[ptr..]);
        ptr += 1;
        let (n, operand) = parse_pratt(&tokens[ptr..], 80)?;
        ptr += n;
        return Ok((
            ptr,
            Expr {
                id: 0,
                kind: ExprKind::Unary {
                    op: UnOp::Neg,
                    operand: Box::new(operand),
                },
                span: unary_span,
            },
        ));
    }

    // Primary atom + optional function application
    let (n, mut lhs) = parse_apply(&tokens[ptr..])?;
    ptr += n;

    // ── Infix loop ────────────────────────────────────────────────────────────
    while let Some(tok) = first_token(&tokens[ptr..]) {
        let (l_bp, r_bp) = match infix_bp(tok) {
            Some(bp) => bp,
            None => break,
        };

        if l_bp < min_bp {
            break;
        }

        // Capture the span of the left operand to use for the binary expr.
        let bin_span = lhs.span.clone();
        let op = token_to_binop(&tokens[ptr].token).unwrap();
        ptr += 1; // consume the operator

        let (n, rhs) = parse_pratt(&tokens[ptr..], r_bp)?;
        ptr += n;

        lhs = Expr {
            id: 0,
            kind: ExprKind::Binary {
                op,
                left: Box::new(lhs),
                right: Box::new(rhs),
            },
            span: bin_span,
        };
    }

    Ok((ptr, lhs))
}

/// Function application: `atom atom*`
/// Left-associative; stops when the next token cannot start an atom.
///
/// Layout rule: if the next argument starts on a *different line* than the
/// last consumed token, stop. This provides basic layout sensitivity so that
/// adjacent identifiers on separate lines are not treated as function
/// application, e.g. `let f = x -> x` followed by `result` on the next line.
fn parse_apply(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    let mut ptr = 0;
    let (n, mut expr) = parse_atom(&tokens[ptr..])?;
    ptr += n;

    // Greedily consume additional atoms as arguments
    loop {
        if !can_start_atom(&tokens[ptr..]) {
            break;
        }
        // Don't consume atoms that look like new top-level bindings
        if matches!(
            first_token(&tokens[ptr..]),
            Some(Token::Let | Token::Type | Token::Use)
        ) {
            break;
        }
        // Layout rule: stop if the argument is on a different line than the
        // last consumed token.
        let last_line = tokens[ptr - 1].span.line;
        let next_line = tokens[ptr].span.line;
        if next_line != last_line {
            break;
        }
        match parse_atom(&tokens[ptr..]) {
            Ok((n, arg)) => {
                let apply_span = expr.span.clone();
                ptr += n;
                expr = Expr {
                    id: 0,
                    kind: ExprKind::Apply {
                        func: Box::new(expr),
                        arg: Box::new(arg),
                    },
                    span: apply_span,
                };
            }
            Err(_) => break,
        }
    }

    Ok((ptr, expr))
}

fn can_start_atom(tokens: &[Spanned]) -> bool {
    matches!(
        first_token(tokens),
        Some(
            Token::Number(_)
                | Token::Text(_)
                | Token::True
                | Token::False
                | Token::Ident(_)
                | Token::TypeIdent(_)
                | Token::LBrace
                | Token::LBracket
                | Token::LParen
                | Token::If // Token::Bar intentionally excluded - match arms as arguments need parens.
        )
    )
}

/// Parse a single atomic expression.
///
/// ```text
/// atom = literal | ident | TypeIdent | record_expr | list_expr
///      | "(" expr ")" | if_expr | match_expr
/// ```
fn parse_atom(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    match first_token(tokens) {
        // ── Typed hole ────────────────────────────────────────────────────────
        Some(Token::Ident(s)) if s == "_" => Ok((
            1,
            Expr {
                id: 0,
                kind: ExprKind::Hole,
                span: span(tokens),
            },
        )),

        // ── Literals ──────────────────────────────────────────────────────────
        Some(Token::Number(n)) => Ok((
            1,
            Expr {
                id: 0,
                kind: ExprKind::Number(*n),
                span: span(tokens),
            },
        )),
        Some(Token::Text(s)) => Ok((
            1,
            Expr {
                id: 0,
                kind: ExprKind::Text(s.clone()),
                span: span(tokens),
            },
        )),
        Some(Token::True) => Ok((
            1,
            Expr {
                id: 0,
                kind: ExprKind::Bool(true),
                span: span(tokens),
            },
        )),
        Some(Token::False) => Ok((
            1,
            Expr {
                id: 0,
                kind: ExprKind::Bool(false),
                span: span(tokens),
            },
        )),

        // ── Identifier ────────────────────────────────────────────────────────
        Some(Token::Ident(name)) => {
            let name = name.clone();
            let ident_span = span(tokens);
            let mut ptr = 1;
            // Field access chain: `alice.name.foo`
            let mut expr = Expr {
                id: 0,
                kind: ExprKind::Ident(name),
                span: ident_span,
            };
            while matches!(first_token(&tokens[ptr..]), Some(Token::Dot)) {
                ptr += 1; // consume `.`
                          // Use the span of the field token for the FieldAccess node so
                          // "field not found" errors point at the field name.
                let field_span = tokens.get(ptr).map(|t| t.span.clone()).unwrap_or_default();
                let (n, field) = consume_ident(&tokens[ptr..])?;
                ptr += n;
                expr = Expr {
                    id: 0,
                    kind: ExprKind::FieldAccess {
                        record: Box::new(expr),
                        field,
                    },
                    span: field_span,
                };
            }
            Ok((ptr, expr))
        }

        // ── Type/variant identifier ───────────────────────────────────────────
        Some(Token::TypeIdent(name)) => {
            let name = name.clone();
            let type_span = span(tokens);
            let mut ptr = 1;

            // Trait call: `Show.show`
            if matches!(first_token(&tokens[ptr..]), Some(Token::Dot)) {
                ptr += 1; // consume `.`
                let method_span = span(&tokens[ptr..]);
                let (n, method_name) = consume_ident(&tokens[ptr..])?;
                ptr += n;
                // Extend the span to cover "TraitName.methodName" so hovering
                // over either part shows the trait call type.
                let full_span = Span {
                    line: type_span.line,
                    col: type_span.col,
                    len: (method_span.col + method_span.len).saturating_sub(type_span.col),
                };
                return Ok((
                    ptr,
                    Expr {
                        id: 0,
                        kind: ExprKind::TraitCall {
                            trait_name: name,
                            method_name,
                        },
                        span: full_span,
                    },
                ));
            }

            // Optional record payload: `Circle { radius: 5 }`
            let payload = if matches!(first_token(&tokens[ptr..]), Some(Token::LBrace)) {
                let (n, rec) = parse_record_expr(&tokens[ptr..])?;
                ptr += n;
                Some(Box::new(rec))
            } else {
                None
            };
            Ok((
                ptr,
                Expr {
                    id: 0,
                    kind: ExprKind::Variant { name, payload },
                    span: type_span,
                },
            ))
        }

        // ── Record / record-update ─────────────────────────────────────────────
        Some(Token::LBrace) => {
            let (n, expr) = parse_record_expr(tokens)?;
            Ok((n, expr))
        }

        // ── List ──────────────────────────────────────────────────────────────
        Some(Token::LBracket) => {
            let (n, expr) = parse_list_expr(tokens)?;
            Ok((n, expr))
        }

        // ── Parenthesised expression ──────────────────────────────────────────
        Some(Token::LParen) => {
            let mut ptr = 1; // consume `(`
            let (n, inner) = parse_expr(&tokens[ptr..])?;
            ptr += n;
            ptr += consume(&tokens[ptr..], &Token::RParen)?;
            Ok((ptr, inner))
        }

        // ── If expression ─────────────────────────────────────────────────────
        Some(Token::If) => {
            let (n, expr) = parse_if(tokens)?;
            Ok((n, expr))
        }

        // ── Match expression (series of `| pat -> body` arms) ────────────────
        Some(Token::Bar) => {
            let (n, expr) = parse_match(tokens)?;
            Ok((n, expr))
        }

        // ── Match-in expression: `match expr in | pat -> body ...` ───────────
        Some(Token::Match) => {
            let (n, expr) = parse_match_expr(tokens)?;
            Ok((n, expr))
        }

        // ── Error ─────────────────────────────────────────────────────────────
        Some(t) => Err(ParseError::unexpected(
            format!("{:?}", t),
            "expression",
            span(tokens),
        )),
        None => Err(ParseError::unexpected_eof(span(tokens))),
    }
}

// ── Record expressions ────────────────────────────────────────────────────────

/// `{ field: value, .. }` or `{ base | field: value }` (update syntax)
fn parse_record_expr(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    let rec_span = span(tokens);
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::LBrace)?;

    // Check for record-update syntax: `{ ident | ... }`
    // Heuristic: if tokens[ptr] is an ident and tokens[ptr+1] is `|`
    let base = if matches!(first_token(&tokens[ptr..]), Some(Token::Ident(_)))
        && matches!(first_token(&tokens[ptr + 1..]), Some(Token::Bar))
    {
        let ident_span = span(&tokens[ptr..]);
        let (n, name) = consume_ident(&tokens[ptr..])?;
        ptr += n;
        ptr += 1; // consume `|`
        Some(Box::new(Expr {
            id: 0,
            kind: ExprKind::Ident(name),
            span: ident_span,
        }))
    } else {
        None
    };

    let mut fields = Vec::new();
    let mut spread = false;

    while !matches!(first_token(&tokens[ptr..]), Some(Token::RBrace) | None) {
        // `..` spread
        if matches!(first_token(&tokens[ptr..]), Some(Token::DotDot)) {
            ptr += 1;
            spread = true;
            // skip trailing comma if any
            if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
                ptr += 1;
            }
            break;
        }

        let (n, field) = parse_record_field(&tokens[ptr..])?;
        ptr += n;
        fields.push(field);

        if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
            ptr += 1;
        } else {
            break;
        }
    }

    ptr += consume(&tokens[ptr..], &Token::RBrace)?;
    Ok((
        ptr,
        Expr {
            id: 0,
            kind: ExprKind::Record {
                base,
                fields,
                spread,
            },
            span: rec_span,
        },
    ))
}

fn parse_record_field(tokens: &[Spanned]) -> Result<(usize, RecordField), ParseError> {
    let mut ptr = 0;
    let name_span = span(tokens);
    // Check if the field name is a constructor (TypeIdent) before consuming.
    let is_constructor = matches!(tokens.first(), Some(Spanned { token: Token::TypeIdent(_), .. }));
    let (n, name) = consume_any_ident(&tokens[ptr..])?;
    ptr += n;

    // Field shorthand: `{ age }` or `{ Circle }` - no colon
    if !matches!(first_token(&tokens[ptr..]), Some(Token::Colon)) {
        // If the field name is an uppercase constructor, synthesize a Variant value
        // so that `pub { Circle }` exports it as a constructor function.
        let value = if is_constructor {
            Some(Expr {
                id: 0,
                kind: ExprKind::Variant { name: name.clone(), payload: None },
                span: name_span.clone(),
            })
        } else {
            None
        };
        return Ok((
            ptr,
            RecordField {
                name,
                name_span,
                name_node_id: 0,
                value,
            },
        ));
    }
    ptr += 1; // consume `:`

    let (n, value) = parse_expr(&tokens[ptr..])?;
    ptr += n;
    Ok((
        ptr,
        RecordField {
            name,
            name_span,
            name_node_id: 0,
            value: Some(value),
        },
    ))
}

// ── List expressions ──────────────────────────────────────────────────────────

/// `[1, 2, 3]`  or  `[]`
fn parse_list_expr(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    let list_span = span(tokens);
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::LBracket)?;

    let mut items = Vec::new();
    while !matches!(first_token(&tokens[ptr..]), Some(Token::RBracket) | None) {
        let (n, item) = parse_expr(&tokens[ptr..])?;
        ptr += n;
        items.push(item);
        if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
            ptr += 1;
        } else {
            break;
        }
    }

    ptr += consume(&tokens[ptr..], &Token::RBracket)?;
    Ok((
        ptr,
        Expr {
            id: 0,
            kind: ExprKind::List(items),
            span: list_span,
        },
    ))
}

// ── If expressions ────────────────────────────────────────────────────────────

/// `if cond then a else b`
fn parse_if(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    let if_span = span(tokens);
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::If)?;

    let (n, cond) = parse_expr(&tokens[ptr..])?;
    ptr += n;

    ptr += consume(&tokens[ptr..], &Token::Then)?;

    let (n, then_branch) = parse_expr(&tokens[ptr..])?;
    ptr += n;

    ptr += consume(&tokens[ptr..], &Token::Else)?;

    let (n, else_branch) = parse_expr(&tokens[ptr..])?;
    ptr += n;

    Ok((
        ptr,
        Expr {
            id: 0,
            kind: ExprKind::If {
                cond: Box::new(cond),
                then_branch: Box::new(then_branch),
                else_branch: Box::new(else_branch),
            },
            span: if_span,
        },
    ))
}

// ── Match expressions ─────────────────────────────────────────────────────────

/// `(| pattern guard? -> body)+`
fn parse_match(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    let match_span = span(tokens);
    let mut ptr = 0;
    let mut arms = Vec::new();

    while matches!(first_token(&tokens[ptr..]), Some(Token::Bar)) {
        ptr += 1; // consume `|`

        let (n, pattern) = parse_pattern(&tokens[ptr..])?;
        ptr += n;

        // optional guard: `if expr`
        let guard = if matches!(first_token(&tokens[ptr..]), Some(Token::If)) {
            ptr += 1; // consume `if`
            let (n, g) = parse_expr(&tokens[ptr..])?;
            ptr += n;
            Some(g)
        } else {
            None
        };

        ptr += consume(&tokens[ptr..], &Token::Arrow)?;

        let (n, body) = parse_expr(&tokens[ptr..])?;
        ptr += n;

        arms.push(MatchArm {
            pattern,
            guard,
            body,
        });
    }

    if arms.is_empty() {
        return Err(ParseError {
            kind: crate::error::ParseErrorKind::EmptyMatch,
            span: match_span.clone(),
        });
    }

    Ok((
        ptr,
        Expr {
            id: 0,
            kind: ExprKind::Match(arms),
            span: match_span,
        },
    ))
}

/// `match expr in | pattern -> body ...`
fn parse_match_expr(tokens: &[Spanned]) -> Result<(usize, Expr), ParseError> {
    let match_span = span(tokens);
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::Match)?;

    // Parse the scrutinee — use the Pratt parser (no lambdas/let-in).
    let (n, scrutinee) = parse_pratt(&tokens[ptr..], 0)?;
    ptr += n;

    ptr += consume(&tokens[ptr..], &Token::In)?;

    // Parse `| pattern -> body` arms (same as parse_match)
    let mut arms = Vec::new();
    while matches!(first_token(&tokens[ptr..]), Some(Token::Bar)) {
        ptr += 1; // consume `|`

        let (n, pattern) = parse_pattern(&tokens[ptr..])?;
        ptr += n;

        let guard = if matches!(first_token(&tokens[ptr..]), Some(Token::If)) {
            ptr += 1;
            let (n, g) = parse_expr(&tokens[ptr..])?;
            ptr += n;
            Some(g)
        } else {
            None
        };

        ptr += consume(&tokens[ptr..], &Token::Arrow)?;

        let (n, body) = parse_expr(&tokens[ptr..])?;
        ptr += n;

        arms.push(MatchArm {
            pattern,
            guard,
            body,
        });
    }

    if arms.is_empty() {
        return Err(ParseError {
            kind: crate::error::ParseErrorKind::EmptyMatch,
            span: match_span.clone(),
        });
    }

    Ok((
        ptr,
        Expr {
            id: 0,
            kind: ExprKind::MatchExpr {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span: match_span,
        },
    ))
}

// ── Patterns ──────────────────────────────────────────────────────────────────

/// Parse a pattern.
///
/// ```text
/// pattern = "_" | literal | ident | VariantName pattern?
///         | record_pattern | list_pattern
/// ```
pub fn parse_pattern(tokens: &[Spanned]) -> Result<(usize, Pattern), ParseError> {
    match first_token(tokens) {
        Some(Token::Ident(s)) if s == "_" => Ok((1, Pattern::Wildcard)),
        Some(Token::Ident(name)) => Ok((1, Pattern::Ident(name.clone(), span(tokens), 0))),
        Some(Token::Number(n)) => Ok((1, Pattern::Literal(Literal::Number(*n)))),
        Some(Token::Text(s)) => Ok((1, Pattern::Literal(Literal::Text(s.clone())))),
        Some(Token::True) => Ok((1, Pattern::Literal(Literal::Bool(true)))),
        Some(Token::False) => Ok((1, Pattern::Literal(Literal::Bool(false)))),

        Some(Token::TypeIdent(name)) => {
            let name = name.clone();
            let mut ptr = 1;
            // optional payload pattern
            let payload = match first_token(&tokens[ptr..]) {
                Some(Token::LBrace) => {
                    let (n, rp) = parse_record_pattern(&tokens[ptr..])?;
                    ptr += n;
                    Some(Box::new(Pattern::Record(rp)))
                }
                Some(Token::LBracket) => {
                    let (n, lp) = parse_list_pattern(&tokens[ptr..])?;
                    ptr += n;
                    Some(Box::new(Pattern::List(lp)))
                }
                // `Variant _`  - wildcard payload without braces
                Some(Token::Ident(s)) if s == "_" => {
                    ptr += 1;
                    Some(Box::new(Pattern::Wildcard))
                }
                // `Variant x` - bind the wrapped value to a name
                Some(Token::Ident(s)) => {
                    let s = s.clone();
                    let pat_span = span(&tokens[ptr..]);
                    ptr += 1;
                    Some(Box::new(Pattern::Ident(s, pat_span, 0)))
                }
                _ => None,
            };
            Ok((ptr, Pattern::Variant { name, payload }))
        }

        Some(Token::LBrace) => {
            let (n, rp) = parse_record_pattern(tokens)?;
            Ok((n, Pattern::Record(rp)))
        }

        Some(Token::LBracket) => {
            let (n, lp) = parse_list_pattern(tokens)?;
            Ok((n, Pattern::List(lp)))
        }

        Some(t) => Err(ParseError::unexpected(
            format!("{:?}", t),
            "pattern",
            span(tokens),
        )),
        None => Err(ParseError::unexpected_eof(span(tokens))),
    }
}

/// `{ name, age: p, .. }`  or  `{ name, ..rest }`
fn parse_record_pattern(tokens: &[Spanned]) -> Result<(usize, RecordPattern), ParseError> {
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::LBrace)?;

    let mut fields = Vec::new();
    let mut rest: Option<Option<String>> = None;

    loop {
        match first_token(&tokens[ptr..]) {
            Some(Token::RBrace) | None => break,
            Some(Token::DotDot) => {
                ptr += 1; // consume `..`
                          // optional name: `..rest`
                let name = if let Some(Token::Ident(s)) = first_token(&tokens[ptr..]) {
                    let s = s.clone();
                    ptr += 1;
                    Some(s)
                } else {
                    None
                };
                rest = Some(name);
                // skip trailing comma
                if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
                    ptr += 1;
                }
                break;
            }
            _ => {
                let (n, fp) = parse_field_pattern(&tokens[ptr..])?;
                ptr += n;
                fields.push(fp);
                if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
                    ptr += 1;
                } else {
                    break;
                }
            }
        }
    }

    ptr += consume(&tokens[ptr..], &Token::RBrace)?;
    Ok((ptr, RecordPattern { fields, rest }))
}

fn parse_field_pattern(tokens: &[Spanned]) -> Result<(usize, FieldPattern), ParseError> {
    let mut ptr = 0;
    let field_span = span(tokens);
    let (n, name) = consume_any_ident(&tokens[ptr..])?;
    ptr += n;

    // optional `: pattern`
    let pattern = if matches!(first_token(&tokens[ptr..]), Some(Token::Colon)) {
        ptr += 1; // consume `:`
        let (n, p) = parse_pattern(&tokens[ptr..])?;
        ptr += n;
        Some(p)
    } else {
        None
    };

    Ok((
        ptr,
        FieldPattern {
            name,
            span: field_span,
            node_id: 0,
            pattern,
        },
    ))
}

/// `[x, y, ..rest]`
fn parse_list_pattern(tokens: &[Spanned]) -> Result<(usize, ListPattern), ParseError> {
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::LBracket)?;

    let mut elements = Vec::new();
    let mut rest: Option<Option<String>> = None;

    loop {
        match first_token(&tokens[ptr..]) {
            Some(Token::RBracket) | None => break,
            Some(Token::DotDot) => {
                ptr += 1; // consume `..`
                let name = if let Some(Token::Ident(s)) = first_token(&tokens[ptr..]) {
                    let s = s.clone();
                    ptr += 1;
                    Some(s)
                } else {
                    None
                };
                rest = Some(name);
                if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
                    ptr += 1;
                }
                break;
            }
            _ => {
                let (n, p) = parse_pattern(&tokens[ptr..])?;
                ptr += n;
                elements.push(p);
                if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
                    ptr += 1;
                } else {
                    break;
                }
            }
        }
    }

    ptr += consume(&tokens[ptr..], &Token::RBracket)?;
    Ok((ptr, ListPattern { elements, rest }))
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Parse a type expression.
///
/// ```text
/// type = TypeName type* | ident | record_type | type "->" type
/// ```
pub fn parse_type(tokens: &[Spanned]) -> Result<(usize, Type), ParseError> {
    let mut ptr = 0;
    let (n, mut ty) = parse_type_atom(&tokens[ptr..])?;
    ptr += n;

    // Right-associative `->` for function types
    if matches!(first_token(&tokens[ptr..]), Some(Token::Arrow)) {
        ptr += 1;
        let (n, ret) = parse_type(&tokens[ptr..])?;
        ptr += n;
        ty = Type::Func {
            param: Box::new(ty),
            ret: Box::new(ret),
        };
    }

    Ok((ptr, ty))
}

/// Parse a single "unapplied" type atom - a type that can appear as an
/// argument without parentheses: a bare type name with no args, a type
/// variable, a record type, or a parenthesized type.
///
/// This is intentionally shallow: `Num`, `Text`, `a`, `{ x: Num }`,
/// `(List Num)` are all valid, but `List Num` is *not* (the `Num` would
/// be a separate token that the caller handles).  This prevents `Result Num
/// Text` from being mis-parsed as `Result (Num Text)`.
fn parse_type_arg(tokens: &[Spanned]) -> Result<(usize, Type), ParseError> {
    match first_token(tokens) {
        Some(Token::TypeIdent(name)) => {
            // Bare type name, no arguments collected here.
            Ok((
                1,
                Type::Named {
                    name: name.clone(),
                    args: vec![],
                },
            ))
        }
        Some(Token::Ident(s)) => Ok((1, Type::Var(s.clone()))),
        Some(Token::LBrace) => {
            let (n, rt) = parse_record_type(tokens)?;
            Ok((n, Type::Record(rt)))
        }
        Some(Token::LParen) => {
            // Parentheses allow a fully-applied type as a single argument,
            // e.g. `Maybe (List Num)`.
            let mut ptr = 1;
            let (n, ty) = parse_type(&tokens[ptr..])?;
            ptr += n;
            ptr += consume(&tokens[ptr..], &Token::RParen)?;
            Ok((ptr, ty))
        }
        Some(t) => Err(ParseError::unexpected(
            format!("{:?}", t),
            "type",
            span(tokens),
        )),
        None => Err(ParseError::unexpected_eof(span(tokens))),
    }
}

fn parse_type_atom(tokens: &[Spanned]) -> Result<(usize, Type), ParseError> {
    match first_token(tokens) {
        Some(Token::TypeIdent(name)) => {
            let name = name.clone();
            let mut ptr = 1;
            // Collect type arguments using parse_type_arg (shallow) so that
            // `Result Num Text` = Con("Result", [Num, Text]) rather than
            // Con("Result", [Con("Num", [Text])]).
            let mut args = Vec::new();
            while let Some(Token::Ident(_) | Token::TypeIdent(_) | Token::LBrace | Token::LParen) =
                first_token(&tokens[ptr..])
            {
                let (n, arg) = parse_type_arg(&tokens[ptr..])?;
                ptr += n;
                args.push(arg);
            }
            Ok((ptr, Type::Named { name, args }))
        }

        Some(Token::Ident(s)) => {
            // Type variable (single lowercase letter or name)
            Ok((1, Type::Var(s.clone())))
        }

        Some(Token::LBrace) => {
            let (n, rt) = parse_record_type(tokens)?;
            Ok((n, Type::Record(rt)))
        }

        Some(Token::LParen) => {
            let mut ptr = 1;
            let (n, ty) = parse_type(&tokens[ptr..])?;
            ptr += n;
            ptr += consume(&tokens[ptr..], &Token::RParen)?;
            Ok((ptr, ty))
        }

        Some(t) => Err(ParseError::unexpected(
            format!("{:?}", t),
            "type",
            span(tokens),
        )),
        None => Err(ParseError::unexpected_eof(span(tokens))),
    }
}

/// `{ name: Text, age: Num, .. }`
fn parse_record_type(tokens: &[Spanned]) -> Result<(usize, RecordType), ParseError> {
    let mut ptr = 0;
    ptr += consume(&tokens[ptr..], &Token::LBrace)?;

    let mut fields = Vec::new();
    let mut open = false;

    loop {
        match first_token(&tokens[ptr..]) {
            Some(Token::RBrace) | None => break,
            Some(Token::DotDot) => {
                ptr += 1;
                open = true;
                if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
                    ptr += 1;
                }
                break;
            }
            _ => {
                let (n, name) = consume_ident(&tokens[ptr..])?;
                ptr += n;
                ptr += consume(&tokens[ptr..], &Token::Colon)?;
                let (n, ty) = parse_type(&tokens[ptr..])?;
                ptr += n;
                fields.push(FieldType { name, ty });
                if matches!(first_token(&tokens[ptr..]), Some(Token::Comma)) {
                    ptr += 1;
                } else {
                    break;
                }
            }
        }
    }

    ptr += consume(&tokens[ptr..], &Token::RBrace)?;
    Ok((ptr, RecordType { fields, open }))
}
