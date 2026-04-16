//! Wadler-Lindig pretty-printer.
//!
//! Build a `Doc`, then call `render(doc, width)` to get a `String`.
//!
//! # Example
//! ```
//! use lume::pretty::{group, nest, text, line, join, render};
//!
//! let doc = group(
//!     nest(2, join(line(), vec![text("a"), text("b"), text("c")]))
//! );
//! assert_eq!(render(doc, 80), "a b c");
//! ```

use std::borrow::Cow;

// ── Document type ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum Doc {
    /// Empty document.
    Nil,
    /// Literal text (must not contain newlines).
    Text(Cow<'static, str>),
    /// Soft line: a space in flat mode, a newline + indentation in break mode.
    Line,
    /// Hard line: always a newline + indentation, even inside `Group`.
    Hardline,
    /// Concatenation of two documents.
    Concat(Box<Doc>, Box<Doc>),
    /// Increase indentation by `n` for the inner document.
    Nest(usize, Box<Doc>),
    /// Try to render on one line; fall back to break mode if it doesn't fit.
    Group(Box<Doc>),
}

// ── Smart constructors ─────────────────────────────────────────────────────────

pub fn nil() -> Doc {
    Doc::Nil
}

pub fn text(s: impl Into<Cow<'static, str>>) -> Doc {
    Doc::Text(s.into())
}

pub fn line() -> Doc {
    Doc::Line
}

pub fn hardline() -> Doc {
    Doc::Hardline
}

pub fn concat(a: Doc, b: Doc) -> Doc {
    match (&a, &b) {
        (Doc::Nil, _) => b,
        (_, Doc::Nil) => a,
        _ => Doc::Concat(Box::new(a), Box::new(b)),
    }
}

pub fn nest(indent: usize, doc: Doc) -> Doc {
    Doc::Nest(indent, Box::new(doc))
}

pub fn group(doc: Doc) -> Doc {
    Doc::Group(Box::new(doc))
}

// ── Combinators ────────────────────────────────────────────────────────────────

/// Concatenate a list of documents left-to-right.
pub fn concat_all(docs: impl IntoIterator<Item = Doc>) -> Doc {
    docs.into_iter().fold(nil(), concat)
}

/// Interleave `sep` between each document in `docs`.
pub fn join(sep: Doc, docs: impl IntoIterator<Item = Doc>) -> Doc {
    let mut iter = docs.into_iter();
    let first = match iter.next() {
        Some(d) => d,
        None => return nil(),
    };
    iter.fold(first, |acc, d| concat(acc, concat(sep.clone(), d)))
}

/// `space()` - a single literal space.
pub fn space() -> Doc {
    text(" ")
}

/// Wrap `doc` in `open` and `close` strings.
pub fn wrap(open: &'static str, close: &'static str, doc: Doc) -> Doc {
    concat(text(open), concat(doc, text(close)))
}

// ── Renderer ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Flat,
    Break,
}

/// Returns `true` if `doc` fits within `remaining` columns when rendered flat.
fn fits(mut remaining: isize, items: &[(usize, Mode, Doc)]) -> bool {
    // We only need to look forward until the doc is exhausted or a hardline appears.
    let mut stack: Vec<(usize, Mode, Doc)> = items.to_vec();
    while let Some((ind, mode, doc)) = stack.pop() {
        if remaining < 0 {
            return false;
        }
        match doc {
            Doc::Nil => {}
            Doc::Text(s) => remaining -= s.len() as isize,
            Doc::Line => match mode {
                Mode::Flat => remaining -= 1, // space
                Mode::Break => return true,   // newline always fits
            },
            Doc::Hardline => return true,
            Doc::Concat(a, b) => {
                stack.push((ind, mode, *b));
                stack.push((ind, mode, *a));
            }
            Doc::Nest(n, inner) => {
                stack.push((ind + n, mode, *inner));
            }
            Doc::Group(inner) => {
                stack.push((ind, Mode::Flat, *inner));
            }
        }
    }
    remaining >= 0
}

/// Render `doc` to a `String`, aiming for lines no longer than `width`.
pub fn render(doc: Doc, width: usize) -> String {
    let mut out = String::new();
    let mut col: usize = 0;
    // Stack items: (indent, mode, doc)
    let mut stack: Vec<(usize, Mode, Doc)> = vec![(0, Mode::Break, doc)];

    while let Some((ind, mode, doc)) = stack.pop() {
        match doc {
            Doc::Nil => {}
            Doc::Text(s) => {
                out.push_str(&s);
                col += s.len();
            }
            Doc::Line => match mode {
                Mode::Flat => {
                    out.push(' ');
                    col += 1;
                }
                Mode::Break => {
                    out.push('\n');
                    out.extend(std::iter::repeat_n(' ', ind));
                    col = ind;
                }
            },
            Doc::Hardline => {
                out.push('\n');
                out.extend(std::iter::repeat_n(' ', ind));
                col = ind;
            }
            Doc::Concat(a, b) => {
                // Push b first so a is processed first.
                stack.push((ind, mode, *b));
                stack.push((ind, mode, *a));
            }
            Doc::Nest(n, inner) => {
                stack.push((ind + n, mode, *inner));
            }
            Doc::Group(inner) => {
                // Peek: will the group fit flat on the current line?
                let remaining = width as isize - col as isize;
                let try_items = vec![(ind, Mode::Flat, (*inner).clone())];
                let chosen_mode = if fits(remaining, &try_items) {
                    Mode::Flat
                } else {
                    Mode::Break
                };
                stack.push((ind, chosen_mode, *inner));
            }
        }
    }

    out
}
