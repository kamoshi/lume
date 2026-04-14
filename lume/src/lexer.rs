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
    And,
    Or,
    Not,

    // Operators
    Arrow,      // ->
    Pipe,       // |>
    ResultPipe, // ?>
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

    Eof,
}

#[derive(Debug, Clone)]
pub struct Spanned {
    pub token: Token,
    pub span: Span,
}

// ── Lexer ─────────────────────────────────────────────────────────────────────

pub struct Lexer<'src> {
    src: &'src [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'src> Lexer<'src> {
    pub fn new(src: &'src str) -> Self {
        Lexer {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
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

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
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
            // Single-line comment: --
            if self.peek() == Some(b'-') && self.peek2() == Some(b'-') {
                while !matches!(self.peek(), Some(b'\n') | None) {
                    self.advance();
                }
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
            b'*' => {
                self.advance();
                Token::Star
            }
            b'/' => {
                self.advance();
                Token::Slash
            }

            // + or ++
            b'+' => {
                self.advance();
                if self.peek() == Some(b'+') {
                    self.advance();
                    Token::Concat
                } else {
                    Token::Plus
                }
            }

            // - or -> or number (negative handled in parser as unary)
            b'-' => {
                self.advance();
                if self.peek() == Some(b'>') {
                    self.advance();
                    Token::Arrow
                } else {
                    Token::Minus
                }
            }

            // = or ==
            b'=' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    Token::EqEq
                } else {
                    Token::Equal
                }
            }

            // ! or !=
            b'!' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    Token::BangEq
                } else {
                    return Err(LexError {
                        kind: LexErrorKind::UnexpectedChar(b'!'),
                        span: self.span_at(line, col, 1),
                    });
                }
            }

            // < or <=
            b'<' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    Token::LtEq
                } else {
                    Token::Lt
                }
            }

            // > or >=
            b'>' => {
                self.advance();
                if self.peek() == Some(b'=') {
                    self.advance();
                    Token::GtEq
                } else {
                    Token::Gt
                }
            }

            // :
            b':' => {
                self.advance();
                Token::Colon
            }

            // | or |>
            b'|' => {
                self.advance();
                if self.peek() == Some(b'>') {
                    self.advance();
                    Token::Pipe
                } else {
                    Token::Bar
                }
            }

            // ? or ?>
            b'?' => {
                self.advance();
                if self.peek() == Some(b'>') {
                    self.advance();
                    Token::ResultPipe
                } else {
                    Token::Question
                }
            }

            // . or ..
            b'.' => {
                self.advance();
                if self.peek() == Some(b'.') {
                    self.advance();
                    Token::DotDot
                } else {
                    Token::Dot
                }
            }

            // String literal
            b'"' => self.lex_string(line, col)?,

            // Number
            b'0'..=b'9' => self.lex_number(),

            // Identifier or keyword
            b'a'..=b'z' | b'_' => self.lex_ident_or_keyword(),

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

    fn lex_ident_or_keyword(&mut self) -> Token {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
        ) {
            self.advance();
        }
        let word = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        match word {
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
            "or" => Token::Or,
            "not" => Token::Not,
            _ => Token::Ident(word.to_string()),
        }
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
