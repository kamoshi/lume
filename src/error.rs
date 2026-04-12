use std::fmt;

// ── Span ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Span {
    pub line: usize,
    pub col: usize,
    pub len: usize,
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

// ── Lex errors ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum LexErrorKind {
    UnexpectedChar(u8),
    UnterminatedString,
}

#[derive(Debug)]
pub struct LexError {
    pub kind: LexErrorKind,
    pub span: Span,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            LexErrorKind::UnexpectedChar(c) => {
                write!(f, "[{}] unexpected character '{}'", self.span, *c as char)
            }
            LexErrorKind::UnterminatedString => {
                write!(f, "[{}] unterminated string literal", self.span)
            }
        }
    }
}

// ── Parse errors ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ParseErrorKind {
    /// Reached the end of the token stream unexpectedly.
    UnexpectedEof,
    /// Got a token we didn't expect in this position.
    UnexpectedToken { found: String, expected: String },
    /// A match expression has zero arms.
    EmptyMatch,
    /// A type definition has no variants.
    EmptyTypeVariants,
}

#[derive(Debug)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub span: Span,
}

impl ParseError {
    pub fn unexpected_eof(span: Span) -> Self {
        ParseError { kind: ParseErrorKind::UnexpectedEof, span }
    }

    pub fn unexpected(found: impl fmt::Display, expected: impl fmt::Display, span: Span) -> Self {
        ParseError {
            kind: ParseErrorKind::UnexpectedToken {
                found: found.to_string(),
                expected: expected.to_string(),
            },
            span,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParseErrorKind::UnexpectedEof => {
                write!(f, "[{}] unexpected end of file", self.span)
            }
            ParseErrorKind::UnexpectedToken { found, expected } => {
                write!(f, "[{}] expected {}, found {}", self.span, expected, found)
            }
            ParseErrorKind::EmptyMatch => {
                write!(f, "[{}] match expression must have at least one arm", self.span)
            }
            ParseErrorKind::EmptyTypeVariants => {
                write!(f, "[{}] type definition must have at least one variant", self.span)
            }
        }
    }
}

// ── Top-level error ───────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum LumeError {
    Lex(LexError),
    Parse(ParseError),
    Type(crate::types::TypeError),
}

impl fmt::Display for LumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LumeError::Lex(e) => write!(f, "lex error: {}", e),
            LumeError::Parse(e) => write!(f, "parse error: {}", e),
            LumeError::Type(e) => write!(f, "type error: {}", e),
        }
    }
}

impl From<LexError> for LumeError {
    fn from(e: LexError) -> Self { LumeError::Lex(e) }
}

impl From<ParseError> for LumeError {
    fn from(e: ParseError) -> Self { LumeError::Parse(e) }
}

impl From<crate::types::TypeError> for LumeError {
    fn from(e: crate::types::TypeError) -> Self { LumeError::Type(e) }
}
