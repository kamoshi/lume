use crate::error::{LexError, LexErrorKind, Span};

// ── Token ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Literals
    Number(f64),
    Text(String),
    True,
    False,

    // Identifiers  (lowercase-start = value, uppercase-start = type/variant)
    Ident(String),
    TypeIdent(String),

    // Keywords
    Let,
    Pub,
    Type,
    Use,
    If,
    Then,
    Else,
    And,       // keyword: mutual recursion separator
    AmpAmp,    // &&
    PipePipe,  // ||
    Not,
    In,
    Trait,     // `trait`
    Match,     // `match`

    // Operators
    Arrow,      // ->
    FatArrow,   // =>
    Pipe,       // |>
    Concat,     // ++
    Plus,       // +
    Minus,      // -
    Star,       // *
    Slash,      // /
    EqEq,       // ==
    BangEq,     // !=
    Lt,         // <
    Gt,         // >
    LtEq,       // <=
    GtEq,       // >=

    // Punctuation
    Equal,    // =
    Colon,    // :
    Bar,      // |
    DotDot,   // ..
    Dot,      // .
    Question, // ?

    // Delimiters
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,

    // Doc comment: --- text
    DocComment(String),

    /// Arbitrary user-defined operator (symbol sequence not matching a built-in token).
    /// Examples: `>>=`, `<*>`, `<$>`, `<|>`
    Operator(String),

    Eof,
}

#[derive(Debug, Clone)]
pub struct Spanned {
    pub token: Token,
    pub span: Span,
}

/// A regular (non-doc) comment captured during lexing.
#[derive(Debug, Clone)]
pub struct Comment {
    /// The comment text (without the leading `-- `).
    pub text: String,
    /// Line number where the comment appeared (1-based).
    pub line: usize,
    /// Column where the `--` started (1-based).
    pub col: usize,
}

// ── Lexer ─────────────────────────────────────────────────────────────────────

pub struct Lexer<'src> {
    src: &'src [u8],
    pos: usize,
    line: usize,
    col: usize,
    comments: Vec<Comment>,
}

impl<'src> Lexer<'src> {
    pub fn new(src: &'src str) -> Self {
        Lexer {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            comments: Vec::new(),
        }
    }

    /// Lex the entire source into a token list (including a final Eof).
    pub fn tokenize(mut self) -> Result<Vec<Spanned>, LexError> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            let is_eof = tok.token == Token::Eof;
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        Ok(tokens)
    }

    /// Lex and return both tokens and captured comments.
    pub fn tokenize_with_comments(mut self) -> Result<(Vec<Spanned>, Vec<Comment>), LexError> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            let is_eof = tok.token == Token::Eof;
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        Ok((tokens, self.comments))
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
    }

    fn peek3(&self) -> Option<u8> {
        self.src.get(self.pos + 2).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let ch = self.src.get(self.pos).copied()?;
        self.pos += 1;
        if ch == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    fn span_at(&self, start_line: usize, start_col: usize, len: usize) -> Span {
        Span {
            line: start_line,
            col: start_col,
            len,
        }
    }

    fn next_token(&mut self) -> Result<Spanned, LexError> {
        // Skip whitespace and comments
        loop {
            // Whitespace
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                self.advance();
            }
            // Doc comment: --- (three dashes)
            if self.peek() == Some(b'-') && self.peek2() == Some(b'-') && self.peek3() == Some(b'-') {
                let doc_line = self.line;
                let doc_col = self.col;
                // skip the ---
                self.advance();
                self.advance();
                self.advance();
                // skip optional leading space
                if self.peek() == Some(b' ') {
                    self.advance();
                }
                let start = self.pos;
                while !matches!(self.peek(), Some(b'\n') | None) {
                    self.advance();
                }
                let text = std::str::from_utf8(&self.src[start..self.pos])
                    .unwrap_or("")
                    .to_string();
                let len = self.pos - start + 3; // approximate span length
                return Ok(Spanned {
                    token: Token::DocComment(text),
                    span: self.span_at(doc_line, doc_col, len),
                });
            }
            // Single-line comment: --
            if self.peek() == Some(b'-') && self.peek2() == Some(b'-') {
                let comment_line = self.line;
                let comment_col = self.col;
                // skip --
                self.advance();
                self.advance();
                // skip optional leading space
                let had_space = self.peek() == Some(b' ');
                if had_space {
                    self.advance();
                }
                let start = self.pos;
                while !matches!(self.peek(), Some(b'\n') | None) {
                    self.advance();
                }
                let text = std::str::from_utf8(&self.src[start..self.pos])
                    .unwrap_or("")
                    .to_string();
                self.comments.push(Comment {
                    text,
                    line: comment_line,
                    col: comment_col,
                });
                continue;
            }
            break;
        }

        let line = self.line;
        let col = self.col;
        let start_pos = self.pos;

        let ch = match self.peek() {
            None => {
                return Ok(Spanned {
                    token: Token::Eof,
                    span: self.span_at(line, col, 0),
                })
            }
            Some(c) => c,
        };

        let token = match ch {
            // Single-char unambiguous
            b'(' => {
                self.advance();
                Token::LParen
            }
            b')' => {
                self.advance();
                Token::RParen
            }
            b'{' => {
                self.advance();
                Token::LBrace
            }
            b'}' => {
                self.advance();
                Token::RBrace
            }
            b'[' => {
                self.advance();
                Token::LBracket
            }
            b']' => {
                self.advance();
                Token::RBracket
            }
            b',' => {
                self.advance();
                Token::Comma
            }

            // - or -> (special: not a general operator char because of comment syntax --)
            b'-' => {
                // Comments (-- and ---) are already consumed before this dispatch.
                // Route into lex_operator so that sequences like -< or -| become operators.
                self.lex_operator()
            }

            // : can appear inside multi-char operators (e.g. ::, <:, :>),
            // but standalone ":" remains Token::Colon.
            b':' => {
                self.lex_operator()
            }

            // Operator characters — greedily collect and classify.
            // '.' is included here: standalone "." and ".." are still reserved tokens,
            // but "." may appear inside multi-char operators (e.g. <.>).
            b'+' | b'*' | b'/' | b'=' | b'!' | b'<' | b'>' | b'|'
            | b'&' | b'?' | b'$' | b'#' | b'@' | b'^' | b'~' | b'%' | b'\\' | b'.' => {
                self.lex_operator()
            }

            // String literal
            b'"' => self.lex_string(line, col)?,

            // Number
            b'0'..=b'9' => self.lex_number(),

            // Identifier or keyword
            b'a'..=b'z' | b'_' => self.lex_ident_or_keyword(line, col)?,

            // Type identifier (uppercase start)
            b'A'..=b'Z' => self.lex_type_ident(),

            other => {
                self.advance();
                return Err(LexError {
                    kind: LexErrorKind::UnexpectedChar(other),
                    span: self.span_at(line, col, 1),
                });
            }
        };

        let len = self.pos - start_pos;
        Ok(Spanned {
            token,
            span: self.span_at(line, col, len),
        })
    }

    /// Lex an operator: greedily consume operator characters, then classify.
    fn lex_operator(&mut self) -> Token {
        let start = self.pos;
        while matches!(self.peek(), Some(b'+' | b'*' | b'/' | b'=' | b'!' | b'<' | b'>'
                                       | b'|' | b'&' | b'?' | b'$' | b'#' | b'@' | b'^' | b'~'
                                       | b'%' | b'\\' | b'.' | b'-' | b':')) {
            // Don't consume "--" or "---" as part of an operator (they're comments).
            // If the next two bytes are "--", stop here.
            if self.peek() == Some(b'-') && self.peek2() == Some(b'-') {
                break;
            }
            self.advance();
        }
        let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        match s {
            "." => Token::Dot,
            ".." => Token::DotDot,
            "-" => Token::Minus,
            "->" => Token::Arrow,
            ":" => Token::Colon,
            "++" => Token::Concat,
            "+" => Token::Plus,
            "*" => Token::Star,
            "/" => Token::Slash,
            "==" => Token::EqEq,
            "!=" => Token::BangEq,
            "=" => Token::Equal,
            "=>" => Token::FatArrow,
            "<" => Token::Lt,
            ">" => Token::Gt,
            "<=" => Token::LtEq,
            ">=" => Token::GtEq,
            "|>" => Token::Pipe,
            "||" => Token::PipePipe,
            "|" => Token::Bar,
            "&&" => Token::AmpAmp,
            "?" => Token::Question,
            _ => Token::Operator(s.to_string()),
        }
    }

    fn lex_string(&mut self, _line: usize, _col: usize) -> Result<Token, LexError> {
        self.advance(); // opening "
        let mut buf = String::new();
        loop {
            match self.advance() {
                None => {
                    return Err(LexError {
                        kind: LexErrorKind::UnterminatedString,
                        span: Span {
                            line: self.line,
                            col: self.col,
                            len: 1,
                        },
                    });
                }
                Some(b'"') => break,
                Some(b'\\') => match self.advance() {
                    Some(b'n') => buf.push('\n'),
                    Some(b't') => buf.push('\t'),
                    Some(b'"') => buf.push('"'),
                    Some(b'\\') => buf.push('\\'),
                    _ => buf.push('\\'),
                },
                Some(c) => buf.push(c as char),
            }
        }
        Ok(Token::Text(buf))
    }

    fn lex_number(&mut self) -> Token {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9' | b'.')) {
            // Avoid consuming `..` as part of a float
            if self.peek() == Some(b'.') && self.peek2() == Some(b'.') {
                break;
            }
            self.advance();
        }
        let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        Token::Number(s.parse().unwrap_or(0.0))
    }

    fn lex_ident_or_keyword(&mut self, line: usize, col: usize) -> Result<Token, LexError> {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
        ) {
            self.advance();
        }
        let word = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        if word.starts_with("__") {
            return Err(LexError {
                kind: LexErrorKind::ReservedIdentifier,
                span: self.span_at(line, col, word.len()),
            });
        }
        Ok(match word {
            "let" => Token::Let,
            "pub" => Token::Pub,
            "type" => Token::Type,
            "use" => Token::Use,
            "if" => Token::If,
            "then" => Token::Then,
            "else" => Token::Else,
            "true" => Token::True,
            "false" => Token::False,
            "and" => Token::And,
            "not" => Token::Not,
            "in" => Token::In,
            "trait" => Token::Trait,
            "match" => Token::Match,
            _ => Token::Ident(word.to_string()),
        })
    }

    fn lex_type_ident(&mut self) -> Token {
        let start = self.pos;
        while matches!(self.peek(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')) {
            self.advance();
        }
        let word = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        Token::TypeIdent(word.to_string())
    }
}
