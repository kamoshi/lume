use std::borrow::Cow;
use std::sync::{Arc, RwLock};

// ── ANSI helpers ──────────────────────────────────────────────────────────────

pub(crate) const RESET: &str = "\x1b[0m";
pub(crate) const BOLD: &str = "\x1b[1m";
pub(crate) const DIM: &str = "\x1b[2m";
const FG_CYAN: &str = "\x1b[36m";
const FG_YELLOW: &str = "\x1b[33m";
const FG_GREEN: &str = "\x1b[32m";
pub(crate) const FG_MAGENTA: &str = "\x1b[35m";
const FG_BLUE: &str = "\x1b[34m";
const FG_RED: &str = "\x1b[31m";

// ── Keywords & completion ─────────────────────────────────────────────────────

pub(crate) const KEYWORDS: &[&str] = &[
    "let", "pub", "type", "use", "if", "then", "else", "and",
    "not", "in", "trait", "match", "true", "false",
];

/// Rebuild the completion list from current defs + static keywords + builtins.
pub(crate) fn refresh_completions(defs: &str, completions: &Arc<RwLock<Vec<String>>>) {
    use lume_core::builtin::BUILTINS;
    use lume_core::lexer::{Lexer, Token};

    let mut names: Vec<String> = KEYWORDS.iter().map(|s| s.to_string()).collect();
    for b in BUILTINS {
        names.push(b.name.to_string());
    }

    if !defs.is_empty() {
        if let Ok(tokens) = Lexer::new(defs).tokenize() {
            let mut prev_was_let = false;
            for tok in &tokens {
                match &tok.token {
                    Token::Let => { prev_was_let = true; }
                    Token::Ident(name) if prev_was_let => {
                        names.push(name.clone());
                        prev_was_let = false;
                    }
                    _ => { prev_was_let = false; }
                }
            }
        }
    }

    names.sort();
    names.dedup();
    *completions.write().unwrap() = names;
}

// ── Syntax highlighting ───────────────────────────────────────────────────────

fn highlight_line(line: &str) -> String {
    use lume_core::lexer::{Lexer, Token};

    if line.starts_with(':') {
        return format!("{FG_MAGENTA}{BOLD}{line}{RESET}");
    }

    let tokens = match Lexer::new(line).tokenize() {
        Ok(t) => t,
        Err(_) => return line.to_string(),
    };

    let mut out = String::with_capacity(line.len() + 64);
    let mut cursor = 0usize;

    for spanned in &tokens {
        let tok = &spanned.token;
        if matches!(tok, Token::Eof) {
            break;
        }

        let span = &spanned.span;
        let col = span.col.saturating_sub(1);
        let len = span.len;

        if col > cursor {
            out.push_str(&line[cursor..col]);
        }
        let end = (col + len).min(line.len());
        let lexeme = &line[col..end];
        cursor = end;

        let colored = match tok {
            Token::Let | Token::Pub | Token::Type | Token::Use
            | Token::If | Token::Then | Token::Else | Token::In
            | Token::Trait | Token::Match | Token::And | Token::Not => {
                format!("{FG_BLUE}{BOLD}{lexeme}{RESET}")
            }
            Token::True | Token::False => format!("{FG_CYAN}{lexeme}{RESET}"),
            Token::TypeIdent(_) => format!("{FG_YELLOW}{lexeme}{RESET}"),
            Token::Text(_) => format!("{FG_GREEN}{lexeme}{RESET}"),
            Token::Number(_) => format!("{FG_CYAN}{lexeme}{RESET}"),
            Token::DocComment(_) => format!("{DIM}{lexeme}{RESET}"),
            Token::Pipe | Token::ResultPipe => format!("{FG_MAGENTA}{lexeme}{RESET}"),
            Token::Arrow | Token::FatArrow => format!("{FG_RED}{lexeme}{RESET}"),
            Token::Plus | Token::Minus | Token::Star | Token::Slash
            | Token::EqEq | Token::BangEq | Token::Lt | Token::Gt
            | Token::LtEq | Token::GtEq | Token::Concat
            | Token::AmpAmp | Token::PipePipe | Token::Equal
            | Token::Colon | Token::Bar | Token::DotDot | Token::Dot => {
                format!("{DIM}{lexeme}{RESET}")
            }
            _ => lexeme.to_string(),
        };
        out.push_str(&colored);
    }

    if cursor < line.len() {
        out.push_str(&line[cursor..]);
    }
    out
}

// ── TypeHint ──────────────────────────────────────────────────────────────────

pub(crate) struct TypeHint(pub String);

impl rustyline::hint::Hint for TypeHint {
    fn display(&self) -> &str { &self.0 }
    fn completion(&self) -> Option<&str> { None }
}

// ── LumeHelper ────────────────────────────────────────────────────────────────

pub(crate) struct LumeHelper {
    pub completions: Arc<RwLock<Vec<String>>>,
    pub defs: Arc<RwLock<String>>,
    pub base_dir: std::path::PathBuf,
}

impl LumeHelper {
    pub(crate) fn new(defs: Arc<RwLock<String>>, base_dir: std::path::PathBuf) -> Self {
        use lume_core::builtin::BUILTINS;
        let mut names: Vec<String> = KEYWORDS.iter().map(|s| s.to_string()).collect();
        for b in BUILTINS {
            names.push(b.name.to_string());
        }
        LumeHelper { completions: Arc::new(RwLock::new(names)), defs, base_dir }
    }

    pub(crate) fn completions_handle(&self) -> Arc<RwLock<Vec<String>>> {
        Arc::clone(&self.completions)
    }
}

impl rustyline::highlight::Highlighter for LumeHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Owned(highlight_line(line))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: rustyline::highlight::CmdKind) -> bool {
        true
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Owned(format!("{FG_MAGENTA}{BOLD}{prompt}{RESET}"))
    }
}

impl rustyline::completion::Completer for LumeHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        // Dot completion takes priority over plain prefix completion.
        if let Some(result) = self.dot_complete(line, pos) {
            return Ok(result);
        }

        let word_start = line[..pos]
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prefix = &line[word_start..pos];

        if prefix.is_empty() {
            return Ok((pos, vec![]));
        }

        let names = self.completions.read().unwrap();
        let mut matches: Vec<String> = names
            .iter()
            .filter(|n| n.starts_with(prefix) && n.as_str() != prefix)
            .cloned()
            .collect();
        matches.sort();
        matches.dedup();

        Ok((word_start, matches))
    }
}

// ── Dot completion helpers ────────────────────────────────────────────────────

impl LumeHelper {
    /// If the cursor is after `<word>.` (with an optional partial identifier
    /// after the dot), return completions for that word's record fields or
    /// trait methods.  Returns `None` when not in a dot-completion context.
    fn dot_complete(&self, line: &str, pos: usize) -> Option<(usize, Vec<String>)> {
        let before = &line[..pos];

        // Find the last `.` that isn't part of `..`
        let dot_pos = before.rfind('.')?;
        if dot_pos > 0 && before.as_bytes()[dot_pos - 1] == b'.' {
            return None; // part of `..`
        }

        // Everything after the dot up to the cursor is the partial field/method.
        let after_dot = &before[dot_pos + 1..];
        if after_dot.chars().any(|c| !c.is_alphanumeric() && c != '_') {
            return None; // contains non-identifier characters — not our case
        }

        // Extract the word immediately before the dot.
        let before_dot = &before[..dot_pos];
        let word_start = before_dot
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|i| i + 1)
            .unwrap_or(0);
        let word = &before_dot[word_start..];
        if word.is_empty() {
            return None;
        }

        let defs = self.defs.read().ok()?;
        let prefix = after_dot;
        let completion_start = dot_pos + 1;

        let candidates = if word.starts_with(|c: char| c.is_uppercase()) {
            trait_methods_for(&defs, word)
        } else {
            record_fields_for(&defs, word, &self.base_dir)
        };

        if candidates.is_empty() {
            return None;
        }

        let mut matches: Vec<String> = candidates
            .into_iter()
            .filter(|m| m.starts_with(prefix) && m.as_str() != prefix)
            .collect();
        matches.sort();

        if matches.is_empty() {
            // If prefix is empty, show all (user typed just `word.`)
            // Re-run without the != prefix filter.
            return None;
        }

        Some((completion_start, matches))
    }
}

/// Return the method names of the named trait as found in `defs`.
fn trait_methods_for(defs: &str, trait_name: &str) -> Vec<String> {
    use lume_core::ast::TopItem;
    use lume_core::lexer::Lexer;
    use lume_core::parser;

    let sep = if defs.is_empty() || defs.ends_with('\n') { "" } else { "\n" };
    let src = format!("{}{}pub {{}}", defs, sep);

    let tokens = match Lexer::new(&src).tokenize() {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let program = match parser::parse_program(&tokens) {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    program.items.iter()
        .filter_map(|item| {
            if let TopItem::TraitDef(td) = item {
                if td.name == trait_name {
                    return Some(
                        td.methods.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
                    );
                }
            }
            None
        })
        .flatten()
        .collect()
}

/// Return the field names of the record type that `var_name` has in `defs`.
fn record_fields_for(defs: &str, var_name: &str, base_dir: &std::path::Path) -> Vec<String> {
    use lume_core::lexer::Lexer;
    use lume_core::parser;
    use lume_core::types::{infer, Ty};

    let sep = if defs.is_empty() || defs.ends_with('\n') { "" } else { "\n" };
    let src = format!("{}{}let _dot_target = {}\n", defs, sep, var_name);

    let tokens = match Lexer::new(&src).tokenize() {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let program = match parser::parse_program(&tokens) {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    let (_, type_env, _, _) = match infer::elaborate_with_env(&program, Some(base_dir)) {
        Ok(r) => r,
        Err(_) => return vec![],
    };

    match type_env.lookup("_dot_target") {
        Some(scheme) => match &scheme.ty {
            Ty::Record(row) => row.fields.iter().map(|(name, _)| name.clone()).collect(),
            _ => vec![],
        },
        None => vec![],
    }
}

impl rustyline::hint::Hinter for LumeHelper {
    type Hint = TypeHint;

    fn hint(&self, line: &str, _pos: usize, _ctx: &rustyline::Context<'_>) -> Option<TypeHint> {
        use lume_core::lexer::Lexer;
        use lume_core::parser;
        use lume_core::types;

        let trimmed = line.trim();

        if trimmed.is_empty()
            || trimmed.starts_with(':')
            || trimmed.starts_with("let ")
            || trimmed.starts_with("type ")
            || trimmed.starts_with("trait ")
            || trimmed.starts_with("use ")
        {
            return None;
        }

        let defs = self.defs.read().ok()?;
        let sep = if defs.is_empty() || defs.ends_with('\n') { "" } else { "\n" };
        let src = format!("{}{}let _repl_hint = {}\n", *defs, sep, trimmed);

        let tokens = Lexer::new(&src).tokenize().ok()?;
        let program = parser::parse_program(&tokens).ok()?;
        let (_, type_env, _, _) = types::infer::elaborate_with_env(&program, Some(&self.base_dir)).ok()?;
        let scheme = type_env.lookup("_repl_hint")?;

        Some(TypeHint(format!("{DIM} : {scheme}{RESET}")))
    }
}

impl rustyline::validate::Validator for LumeHelper {
    fn validate(
        &self,
        ctx: &mut rustyline::validate::ValidationContext,
    ) -> rustyline::Result<rustyline::validate::ValidationResult> {
        use lume_core::error::ParseErrorKind;
        use lume_core::lexer::Lexer;
        use lume_core::parser;

        let input = ctx.input();
        if input.starts_with(':') {
            return Ok(rustyline::validate::ValidationResult::Valid(None));
        }

        let src = format!("{input}\npub {{}}\n");
        let result = Lexer::new(&src)
            .tokenize()
            .map_err(|_| ())
            .and_then(|tokens| parser::parse_program(&tokens).map_err(|_| ()));

        match result {
            Ok(_) => Ok(rustyline::validate::ValidationResult::Valid(None)),
            Err(_) => {
                let src2 = format!("{input}\npub {{}}\n");
                let is_eof = Lexer::new(&src2)
                    .tokenize()
                    .ok()
                    .and_then(|tokens| parser::parse_program(&tokens).err())
                    .map(|e| matches!(e.kind, ParseErrorKind::UnexpectedEof))
                    .unwrap_or(false);
                if is_eof {
                    Ok(rustyline::validate::ValidationResult::Incomplete)
                } else {
                    Ok(rustyline::validate::ValidationResult::Valid(None))
                }
            }
        }
    }
}

impl rustyline::Helper for LumeHelper {}
