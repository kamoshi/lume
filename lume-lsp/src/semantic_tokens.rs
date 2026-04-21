use lume_core::lexer::{Lexer, Token};
use tower_lsp::lsp_types::*;

use crate::analysis::{DocInfo, SemanticClass, SemanticMod};

// ── Semantic token type legend ────────────────────────────────────────────────
//
// The indices here MUST match the order in `LEGEND_TOKEN_TYPES`.

const TY_KEYWORD: u32 = 0;
const TY_VARIABLE: u32 = 1;
const TY_TYPE: u32 = 2;
const TY_NUMBER: u32 = 3;
const TY_STRING: u32 = 4;
const TY_OPERATOR: u32 = 5;
const TY_COMMENT: u32 = 6;
const TY_FUNCTION: u32 = 7;
const TY_METHOD: u32 = 8;
const TY_ENUM_MEMBER: u32 = 9;
const TY_PARAMETER: u32 = 10;
const TY_PROPERTY: u32 = 11;
const TY_TYPE_PARAMETER: u32 = 12;

pub const LEGEND_TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,        // 0
    SemanticTokenType::VARIABLE,       // 1
    SemanticTokenType::TYPE,           // 2
    SemanticTokenType::NUMBER,         // 3
    SemanticTokenType::STRING,         // 4
    SemanticTokenType::OPERATOR,       // 5
    SemanticTokenType::COMMENT,        // 6
    SemanticTokenType::FUNCTION,       // 7
    SemanticTokenType::METHOD,         // 8
    SemanticTokenType::ENUM_MEMBER,    // 9
    SemanticTokenType::PARAMETER,      // 10
    SemanticTokenType::PROPERTY,       // 11
    SemanticTokenType::TYPE_PARAMETER, // 12
];

// ── Semantic token modifier legend ───────────────────────────────────────────

const MOD_DECLARATION: u32 = 1 << 0;
const MOD_READONLY: u32 = 1 << 1;

pub const LEGEND_TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION, // bit 0
    SemanticTokenModifier::READONLY,    // bit 1
];

// ── Internal token representation ────────────────────────────────────────────

#[derive(Debug, Clone)]
struct RawToken {
    line: u32,
    col: u32,
    len: u32,
    token_type: u32,
    token_mods: u32,
}

impl RawToken {
    fn pos_key(&self) -> (u32, u32) {
        (self.line, self.col)
    }
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Build the full semantic token list for a Lume source file.
///
/// Uses a two-pass approach:
/// 1. **Lexer pass** — classifies every token syntactically (keywords, literals,
///    operators, comments).
/// 2. **Semantic override pass** — uses the typed AST (via `DocInfo`) to upgrade
///    identifier tokens to richer classes: `function`, `method`, `enumMember`,
///    `parameter`, `property`.  Declaration sites get the `declaration` modifier.
///    All Lume bindings are immutable, so every binding gets `readonly`.
///
/// The result is delta-encoded per the LSP spec.
pub fn compute_semantic_tokens(src: &str, doc: Option<&DocInfo>) -> Vec<SemanticToken> {
    // Pass 1: lex and classify tokens.
    let mut raw = lex_tokens(src);

    // Pass 2: apply semantic overrides from the typed AST.
    if let Some(doc) = doc {
        apply_semantic_overrides(&mut raw, &doc.semantic_spans);
    }

    // Sort and delta-encode.
    raw.sort_by(|a, b| a.line.cmp(&b.line).then(a.col.cmp(&b.col)));
    delta_encode(raw)
}

// ── Lexer pass ────────────────────────────────────────────────────────────────

fn lex_tokens(src: &str) -> Vec<RawToken> {
    let mut raw: Vec<RawToken> = Vec::new();

    if let Ok(tokens) = Lexer::new(src).tokenize() {
        for sp in &tokens {
            let ty = match &sp.token {
                // Keywords
                Token::Let | Token::Pub | Token::Type | Token::Use
                | Token::If | Token::Then | Token::Else | Token::And
                | Token::Not | Token::In | Token::Trait | Token::Match => TY_KEYWORD,

                // Bool literals — treat as keyword for better visual distinction
                Token::True | Token::False => TY_KEYWORD,

                // Identifiers — `infix`/`infixl`/`infixr` are plain Ident tokens;
                // we classify them in the semantic override pass if needed.
                Token::Ident(name) => match name.as_str() {
                    "infix" | "infixl" | "infixr" => TY_KEYWORD,
                    _ => TY_VARIABLE,
                },

                // Type / variant identifiers (UpperCamelCase)
                Token::TypeIdent(_) => TY_TYPE,

                // Numeric literals
                Token::Number(_) => TY_NUMBER,

                // String literals
                Token::Text(_) => TY_STRING,

                // Built-in operators
                Token::Arrow | Token::FatArrow | Token::Pipe
                | Token::Concat | Token::Plus | Token::Minus | Token::Star
                | Token::Slash | Token::EqEq | Token::BangEq | Token::Lt
                | Token::Gt | Token::LtEq | Token::GtEq | Token::AmpAmp
                | Token::PipePipe | Token::DotDot => TY_OPERATOR,

                // Custom operators (user-defined via infix/infixl/infixr)
                Token::Operator(_) => TY_OPERATOR,

                // Doc comments
                Token::DocComment(_) => TY_COMMENT,

                // Punctuation / delimiters / Eof → skip
                _ => continue,
            };

            raw.push(RawToken {
                line: sp.span.line.saturating_sub(1) as u32,
                col: sp.span.col.saturating_sub(1) as u32,
                len: sp.span.len as u32,
                token_type: ty,
                token_mods: 0,
            });
        }
    }

    // Scan for `--` line comments (the lexer silently skips them).
    scan_line_comments(src, &mut raw);

    raw
}

// ── Semantic override pass ────────────────────────────────────────────────────

fn apply_semantic_overrides(
    raw: &mut Vec<RawToken>,
    semantic_spans: &[crate::analysis::SemanticSpan],
) {
    use std::collections::HashMap;

    // Build a position → index map for quick lookup.
    let mut pos_map: HashMap<(u32, u32), usize> = HashMap::with_capacity(raw.len());
    for (i, tok) in raw.iter().enumerate() {
        pos_map.insert(tok.pos_key(), i);
    }

    for ss in semantic_spans {
        if ss.span.len == 0 {
            continue;
        }
        let line = ss.span.line.saturating_sub(1) as u32;
        let col = ss.span.col.saturating_sub(1) as u32;

        let token_type = semantic_class_to_type(&ss.class);
        let token_mods = semantic_mods_to_bits(ss.mods);

        if let Some(&idx) = pos_map.get(&(line, col)) {
            raw[idx].token_type = token_type;
            raw[idx].token_mods = token_mods;
        } else {
            // The AST has a span that the lexer didn't emit a token for.
            // Insert it as a new token.
            raw.push(RawToken {
                line,
                col,
                len: ss.span.len as u32,
                token_type,
                token_mods,
            });
        }
    }
}

fn semantic_class_to_type(class: &SemanticClass) -> u32 {
    match class {
        SemanticClass::Function => TY_FUNCTION,
        SemanticClass::Variable => TY_VARIABLE,
        SemanticClass::Parameter => TY_PARAMETER,
        SemanticClass::EnumMember => TY_ENUM_MEMBER,
        SemanticClass::Method => TY_METHOD,
        SemanticClass::Property => TY_PROPERTY,
        SemanticClass::Type => TY_TYPE,
        SemanticClass::TypeParameter => TY_TYPE_PARAMETER,
        SemanticClass::Operator => TY_OPERATOR,
    }
}

fn semantic_mods_to_bits(mods: SemanticMod) -> u32 {
    let mut bits = 0u32;
    if mods.contains(SemanticMod::DECLARATION) {
        bits |= MOD_DECLARATION;
    }
    if mods.contains(SemanticMod::READONLY) {
        bits |= MOD_READONLY;
    }
    bits
}

// ── Delta encoding ────────────────────────────────────────────────────────────

fn delta_encode(mut raw: Vec<RawToken>) -> Vec<SemanticToken> {
    raw.sort_by(|a, b| a.line.cmp(&b.line).then(a.col.cmp(&b.col)));

    let mut out = Vec::with_capacity(raw.len());
    let mut prev_line = 0u32;
    let mut prev_col = 0u32;

    for tok in &raw {
        let delta_line = tok.line - prev_line;
        let delta_start = if delta_line == 0 {
            tok.col - prev_col
        } else {
            tok.col
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: tok.len,
            token_type: tok.token_type,
            token_modifiers_bitset: tok.token_mods,
        });
        prev_line = tok.line;
        prev_col = tok.col;
    }

    out
}

// ── Comment scanner ───────────────────────────────────────────────────────────

/// Find `--` line comments that the lexer skipped.
///
/// Walks the source tracking string literals to avoid false positives
/// on `"--"` inside strings.  Doc comments (`---`) are already emitted
/// by the lexer so they are excluded here.
fn scan_line_comments(src: &str, out: &mut Vec<RawToken>) {
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut line: u32 = 0;
    let mut col: u32 = 0;
    let mut in_string = false;

    while i < bytes.len() {
        match bytes[i] {
            b'"' if !in_string => {
                in_string = true;
                i += 1;
                col += 1;
            }
            b'"' if in_string => {
                in_string = false;
                i += 1;
                col += 1;
            }
            b'\\' if in_string && i + 1 < bytes.len() => {
                i += 2;
                col += 2;
            }
            b'-' if !in_string && i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                let start_col = col;
                let start_i = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                    col += 1;
                }
                let comment_len = (i - start_i) as u32;
                // Doc comments (---) are already emitted by the lexer.
                let is_doc =
                    comment_len >= 3 && start_i + 2 < bytes.len() && bytes[start_i + 2] == b'-';
                if !is_doc {
                    out.push(RawToken {
                        line,
                        col: start_col,
                        len: comment_len,
                        token_type: TY_COMMENT,
                        token_mods: 0,
                    });
                }
            }
            b'\n' => {
                line += 1;
                col = 0;
                i += 1;
                in_string = false;
            }
            _ => {
                i += 1;
                col += 1;
            }
        }
    }
}
