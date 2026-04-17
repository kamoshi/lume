use lume::lexer::{Lexer, Token};
use tower_lsp::lsp_types::*;

pub const LEGEND_TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,  // 0
    SemanticTokenType::VARIABLE, // 1
    SemanticTokenType::TYPE,     // 2
    SemanticTokenType::NUMBER,   // 3
    SemanticTokenType::STRING,   // 4
    SemanticTokenType::OPERATOR, // 5
    SemanticTokenType::COMMENT,  // 6
];

struct RawSemanticToken {
    line: u32,
    col: u32,
    len: u32,
    token_type: u32,
}

/// Build the complete list of semantic tokens for a Lume source file.
///
/// Uses the lexer token stream for keywords, identifiers, type identifiers,
/// literals, and operators.  Adds a separate scan for `--` line comments
/// (which the lexer consumes without emitting).
pub fn compute_semantic_tokens(src: &str) -> Vec<SemanticToken> {
    let mut raw: Vec<RawSemanticToken> = Vec::new();

    // 1. Lex the source and classify each token.
    if let Ok(tokens) = Lexer::new(src).tokenize() {
        for sp in &tokens {
            let ty = match &sp.token {
                // Keywords
                Token::Let | Token::Pub | Token::Type | Token::Use
                | Token::If | Token::Then | Token::Else | Token::And
                | Token::Not | Token::In | Token::Trait | Token::Match
                | Token::True | Token::False => 0,

                // Identifiers
                Token::Ident(_) => 1,

                // Type / variant identifiers
                Token::TypeIdent(_) => 2,

                // Numeric literals
                Token::Number(_) => 3,

                // String literals
                Token::Text(_) => 4,

                // Operators
                Token::Arrow | Token::FatArrow | Token::Pipe | Token::ResultPipe
                | Token::Concat | Token::Plus | Token::Minus | Token::Star
                | Token::Slash | Token::EqEq | Token::BangEq | Token::Lt
                | Token::Gt | Token::LtEq | Token::GtEq | Token::AmpAmp
                | Token::PipePipe => 5,

                // Doc comments (--- ...)
                Token::DocComment(_) => 6,

                // Punctuation / delimiters / Eof → skip
                _ => continue,
            };

            raw.push(RawSemanticToken {
                line: sp.span.line.saturating_sub(1) as u32,
                col: sp.span.col.saturating_sub(1) as u32,
                len: sp.span.len as u32,
                token_type: ty,
            });
        }
    }

    // 2. Scan for regular `--` line comments (the lexer silently skips them).
    scan_line_comments(src, &mut raw);

    // 3. Sort by position.
    raw.sort_by(|a, b| a.line.cmp(&b.line).then(a.col.cmp(&b.col)));

    // 4. Delta-encode for the LSP wire format.
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
            token_modifiers_bitset: 0,
        });
        prev_line = tok.line;
        prev_col = tok.col;
    }

    out
}

/// Find `--` line comments that the lexer skipped.
///
/// Walks the source tracking string literals to avoid false positives
/// on `"--"` inside strings.  Doc comments (`---`) are already emitted
/// by the lexer so they are excluded here.
fn scan_line_comments(src: &str, out: &mut Vec<RawSemanticToken>) {
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
                    out.push(RawSemanticToken {
                        line,
                        col: start_col,
                        len: comment_len,
                        token_type: 6,
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
