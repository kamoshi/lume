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

// ── String-aware scanning ──────────────────────────────────────────────────────

/// Find a substring in `line` while skipping regions inside string literals.
/// Returns the byte offset of the first occurrence outside quotes, or `None`.
fn find_outside_string(line: &str, needle: &str) -> Option<usize> {
    find_nth_outside_string(line, needle, 0)
}

/// Find the nth (0-based) occurrence of `needle` outside string literals.
fn find_nth_outside_string(line: &str, needle: &str, n: usize) -> Option<usize> {
    let mut count = 0;
    let bytes = line.as_bytes();
    let nlen = needle.len();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            // Skip over string literal contents.
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1; // skip escaped char
                }
                i += 1;
            }
            i += 1; // skip closing quote
        } else if i + nlen <= bytes.len() && &line[i..i + nlen] == needle {
            if count == n {
                return Some(i);
            }
            count += 1;
            i += nlen;
        } else {
            i += 1;
        }
    }
    None
}

/// Find the rightmost occurrence of `needle` outside string literals.
fn rfind_outside_string(line: &str, needle: &str) -> Option<usize> {
    let mut last = None;
    let bytes = line.as_bytes();
    let nlen = needle.len();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
        } else if i + nlen <= bytes.len() && &line[i..i + nlen] == needle {
            last = Some(i);
            i += nlen;
        } else {
            i += 1;
        }
    }
    last
}

/// Check if `line` contains `needle` outside string literals.
fn contains_outside_string(line: &str, needle: &str) -> bool {
    find_outside_string(line, needle).is_some()
}

/// Split a line on ` |> ` boundaries outside string literals,
/// keeping the operator with the right-hand segment.
fn split_on_pipes_aware(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut last = 0;
    let bytes = s.as_bytes();
    let len = s.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'"' {
            i += 1;
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
        } else if i + 3 < len
            && bytes[i] == b' '
            && (bytes[i + 1] == b'|' || bytes[i + 1] == b'?')
            && bytes[i + 2] == b'>'
            && bytes[i + 3] == b' '
        {
            parts.push(&s[last..i]);
            last = i + 1; // start from the operator
            i += 4;
        } else {
            i += 1;
        }
    }
    parts.push(&s[last..]);
    parts
}

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

/// Entry kinds for the flat-list formatting approach.
#[derive(Clone, Copy, PartialEq)]
enum EntryKind { Use, Item, Export, Comment }

/// A formatted entry (use, item, comment, or export) with source position.
struct FormatEntry {
    kind: EntryKind,
    source_line: usize, // 1-based
    text: String,
    /// True for end-of-line comments (code precedes the comment on same source line).
    is_eol_comment: bool,
}

/// Parse and format source text with full AST reformatting.
/// Comments are preserved by building a flat list of anchors (items + comments)
/// sorted by source position, then emitting them with spacing derived from the
/// original source.  Returns `None` if parsing fails.
pub fn format_source(src: &str, config: &FormatConfig) -> Option<String> {
    let lexer = crate::lexer::Lexer::new(src);
    let (tokens, comments) = lexer.tokenize_with_comments().ok()?;
    let mut program = crate::parser::parse_program(&tokens).ok()?;
    program.pragmas = crate::loader::parse_pragmas(src).0;

    if comments.is_empty() {
        return Some(format_program(&program, config));
    }

    let src_lines: Vec<&str> = src.lines().collect();
    let ind = config.indent;

    let mut entries: Vec<FormatEntry> = Vec::new();

    // Uses
    for u in &program.uses {
        let line = use_start_line(u);
        let text = render(fmt_use(u), config.width).trim_end().to_string();
        entries.push(FormatEntry { kind: EntryKind::Use, source_line: line, text, is_eol_comment: false });
    }

    // Top-level items — conditionally apply match-arm arrow alignment.
    let item_starts: Vec<usize> = program.items.iter().map(top_item_start_line).collect();
    for (idx, item) in program.items.iter().enumerate() {
        let line = item_starts[idx];
        let end_line = item_starts.get(idx + 1).copied()
            .or_else(|| {
                if let ExprKind::Record { entries: es, .. } = &program.exports.kind {
                    if !es.is_empty() { return Some(program.exports.span.line); }
                }
                None
            })
            .unwrap_or(src_lines.len() + 1);
        let raw = render(fmt_top_item(item, ind), config.width).trim_end().to_string();
        let text = preserve_alignment(&raw, &src_lines, line, end_line);
        entries.push(FormatEntry { kind: EntryKind::Item, source_line: line, text, is_eol_comment: false });
    }

    // Pub exports
    if let ExprKind::Record { entries: es, .. } = &program.exports.kind {
        if !es.is_empty() {
            let line = program.exports.span.line;
            let text = render(fmt_pub_exports(es, ind), config.width).trim_end().to_string();
            entries.push(FormatEntry { kind: EntryKind::Export, source_line: line, text, is_eol_comment: false });
        }
    }

    // Comments — distinguish standalone from end-of-line.
    for c in &comments {
        let text = if c.text.is_empty() {
            "--".to_string()
        } else {
            format!("-- {}", c.text)
        };
        // EOL comment: something non-whitespace exists before the `--` on this line.
        let is_eol = if let Some(src_line) = src_lines.get(c.line.saturating_sub(1)) {
            let prefix = &src_line[..src_line.len().min(c.col.saturating_sub(1))];
            !prefix.trim().is_empty()
        } else {
            false
        };
        entries.push(FormatEntry { kind: EntryKind::Comment, source_line: c.line, text, is_eol_comment: is_eol });
    }

    // Sort by source line. Standalone comments go before code on the same line;
    // end-of-line comments go after.
    entries.sort_by(|a, b| {
        a.source_line.cmp(&b.source_line).then_with(|| {
            let pri = |e: &FormatEntry| {
                if e.kind == EntryKind::Comment && !e.is_eol_comment { 0 }
                else if e.kind != EntryKind::Comment { 1 }
                else { 2 } // EOL comment goes after code
            };
            pri(a).cmp(&pri(b))
        })
    });

    // Walk entries, emitting with appropriate blank-line spacing.
    let mut result = String::with_capacity(src.len() + 256);
    let has_uses = entries.iter().any(|e| e.kind == EntryKind::Use);
    let mut seen_non_use = false;

    // Align `=` in groups of consecutive single-line Item bindings
    // where the source had them aligned.
    align_binding_equals(&mut entries, &src_lines);

    for (i, entry) in entries.iter().enumerate() {
        // End-of-line comments are appended to the previous line.
        if entry.is_eol_comment {
            // Remove the trailing newline from the previous output, append the comment.
            if result.ends_with('\n') {
                result.pop();
            }
            result.push_str("  ");
            result.push_str(&entry.text);
            result.push('\n');
            continue;
        }

        if i > 0 {
            let src_blanks = count_preceding_blanks(&src_lines, entry.source_line);
            let entering_items = has_uses && !seen_non_use && entry.kind != EntryKind::Use;
            let before_export = entry.kind == EntryKind::Export;
            let min_blanks = if entering_items || before_export { 2 } else { 0 };
            let blanks = src_blanks.max(min_blanks).min(2);
            set_trailing_blank_lines(&mut result, blanks);
        }

        result.push_str(&entry.text);
        result.push('\n');

        if entry.kind != EntryKind::Use {
            seen_non_use = true;
        }
    }

    // Ensure single trailing newline.
    while result.ends_with("\n\n") {
        result.pop();
    }
    if !result.ends_with('\n') {
        result.push('\n');
    }

    Some(result)
}

/// Ensure the string ends with exactly `n` blank lines (= `n+1` newlines).
fn set_trailing_blank_lines(s: &mut String, n: usize) {
    // Strip all trailing newlines.
    while s.ends_with('\n') {
        s.pop();
    }
    // Add n+1 newlines: one to end the last content line, plus n blank lines.
    for _ in 0..=n {
        s.push('\n');
    }
}

/// Count consecutive blank source lines immediately before `line_1based`.
fn count_preceding_blanks(src_lines: &[&str], line_1based: usize) -> usize {
    let mut count = 0;
    let mut l = line_1based.saturating_sub(1); // 0-based index of line before target
    while l > 0 {
        l -= 1;
        if src_lines.get(l).map(|s| s.trim().is_empty()).unwrap_or(false) {
            count += 1;
        } else {
            break;
        }
    }
    count
}

/// Source line of a `use` declaration (1-based).
fn use_start_line(u: &UseDecl) -> usize {
    match &u.binding {
        UseBinding::Ident(_, span, _) => span.line,
        UseBinding::Record(rp) => rp.fields.first().map(|f| f.span.line).unwrap_or(1),
    }
}

/// Source line of a top-level item (1-based).
fn top_item_start_line(item: &TopItem) -> usize {
    match item {
        TopItem::TypeDef(td) => td.name_span.line,
        TopItem::Binding(b) => binding_start_line(b),
        TopItem::BindingGroup(bs) => bs.first().map(binding_start_line).unwrap_or(1),
        TopItem::TraitDef(td) => td.name_span.line,
        TopItem::ImplDef(id) => id.trait_name_span.line,
    }
}

/// Source line of a binding (1-based), derived from the pattern identifier.
fn binding_start_line(b: &Binding) -> usize {
    match &b.pattern {
        Pattern::Ident(_, span, _) => span.line,
        _ => b.value.span.line,
    }
}

// ── Alignment preservation ─────────────────────────────────────────────────────

/// Post-process formatted item text:
/// 1. Break lambda bodies to next line when source had them there.
/// 2. Expand horizontal pipe chains to vertical when source had vertical pipes.
/// 3. Preserve match-arm `->` alignment from source.
fn preserve_alignment(formatted: &str, src_lines: &[&str], item_start: usize, item_end: usize) -> String {
    // Step 1: break lambda body to next line if source had it there.
    let after_lambda = preserve_lambda_body_break(formatted, src_lines, item_start);

    // Step 2: expand pipes if source used vertical style.
    let after_pipes = if source_has_vertical_pipes(src_lines, item_start, item_end) {
        expand_pipes_vertical(&after_lambda)
    } else {
        after_lambda
    };

    // Step 3: align match-arm arrows.
    let fmt_lines: Vec<&str> = after_pipes.lines().collect();
    let mut result_lines: Vec<String> = fmt_lines.iter().map(|l| l.to_string()).collect();

    let mut i = 0;
    while i < fmt_lines.len() {
        if is_match_arm_line(fmt_lines[i]) {
            let group_start = i;
            while i < fmt_lines.len() && is_match_arm_line(fmt_lines[i]) {
                i += 1;
            }
            let group_end = i;
            if group_end - group_start >= 2
                && source_arms_aligned(src_lines, item_start, &fmt_lines[group_start..group_end])
            {
                align_arrows(&mut result_lines, group_start, group_end);
            }
        } else {
            i += 1;
        }
    }

    let mut out = result_lines.join("\n");
    if formatted.ends_with('\n') && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// If the source had a lambda body on the next line (source line ends with `->`)
/// but the formatted output has it inline, break it.
fn preserve_lambda_body_break(formatted: &str, src_lines: &[&str], item_start: usize) -> String {
    let src_line = src_lines.get(item_start.saturating_sub(1)).unwrap_or(&"");
    let src_trimmed = src_line.trim_end();

    // Check if source line ends with `->`
    if !src_trimmed.ends_with("->") {
        return formatted.to_string();
    }

    let fmt_lines: Vec<&str> = formatted.lines().collect();
    if fmt_lines.is_empty() {
        return formatted.to_string();
    }

    // The formatted first line might have everything inline: `let x = a -> body`
    // We want to break after the last `->` on this line.
    let first = fmt_lines[0];
    // Find the last ` -> ` that is followed by non-pipe, non-`|` content.
    // We want to split: `let process = x -> x |> compute |> abs`
    // into: `let process = x ->\n  x |> compute |> abs`
    if let Some(arrow_pos) = find_last_lambda_arrow(first) {
        let before = &first[..arrow_pos + 3]; // includes ` ->`
        let after = first[arrow_pos + 4..].trim_start(); // skip ` -> `
        if after.starts_with("-> ") {
            // This was a `-> ` break point inside params, skip
            return formatted.to_string();
        }
        if !after.is_empty() && fmt_lines.len() == 1 {
            // Single-line binding with body after `->`, break it.
            let indent = "  ";
            let mut result = String::new();
            result.push_str(before);
            result.push('\n');
            result.push_str(indent);
            result.push_str(after);
            if formatted.ends_with('\n') {
                result.push('\n');
            }
            return result;
        }
    }

    formatted.to_string()
}

/// Find the position of the last ` -> ` in a line that represents a lambda arrow,
/// skipping occurrences inside string literals.
/// Returns the byte position of the space before `->`.
fn find_last_lambda_arrow(line: &str) -> Option<usize> {
    rfind_outside_string(line, " -> ").filter(|&pos| {
        let after = &line[pos + 4..];
        !after.is_empty()
    })
}

/// Check if source lines for this item (between item_start and item_end, 1-based)
/// contain `|>` at the start of a line (after whitespace).
fn source_has_vertical_pipes(src_lines: &[&str], item_start: usize, item_end: usize) -> bool {
    let start_idx = item_start.saturating_sub(1);
    let end_idx = item_end.min(src_lines.len());
    src_lines[start_idx..end_idx].iter().any(|l| {
        let t = l.trim_start();
        t.starts_with("|>")
    })
}

/// Expand horizontal pipe chains into vertical format.
/// For each line containing ` |> ` outside string literals, split so
/// each pipe step is on its own line indented 2 more than the base expression.
fn expand_pipes_vertical(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + 64);
    for line in text.lines() {
        if contains_outside_string(line, " |> ") {
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            let content = line.trim_start();

            let parts = split_on_pipes_aware(content);
            if parts.len() > 1 {
                result.push_str(indent);
                result.push_str(parts[0].trim_end());
                result.push('\n');
                for part in &parts[1..] {
                    result.push_str(indent);
                    result.push_str("  ");
                    result.push_str(part.trim());
                    result.push('\n');
                }
                continue;
            }
        }
        result.push_str(line);
        result.push('\n');
    }
    // Remove the trailing extra newline we added.
    if result.ends_with('\n') && !text.ends_with('\n') {
        result.pop();
    }
    result
}

/// A line is a match arm if its trimmed form starts with `| ` and contains ` -> `
/// outside string literals.
fn is_match_arm_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("| ") && contains_outside_string(t, " -> ")
}

/// Check if the source lines corresponding to this arm group had aligned `->`.
/// We find source arms by scanning from `item_start` (1-based) looking for
/// `| ... -> ...` lines.
fn source_arms_aligned(src_lines: &[&str], item_start: usize, fmt_arms: &[&str]) -> bool {
    // Collect all match-arm lines from the item's source region.
    let start_idx = item_start.saturating_sub(1);
    let src_arms: Vec<(usize, &str)> = src_lines[start_idx..]
        .iter()
        .enumerate()
        .filter(|(_, l)| is_match_arm_line(l))
        .map(|(i, l)| (i, *l))
        .collect();

    // Find the group in source arms that matches this formatter group.
    // We match by checking the pattern text (before `->`) after stripping whitespace.
    let fmt_patterns: Vec<String> = fmt_arms
        .iter()
        .map(|l| {
            let t = l.trim_start();
            // Split at first ` -> ` outside string literals.
            let pat = if let Some(pos) = find_outside_string(t, " -> ") {
                &t[..pos]
            } else {
                t
            };
            pat.replace(' ', "")
        })
        .collect();

    // Find a contiguous subsequence of source arms matching fmt_patterns.
    'outer: for window_start in 0..src_arms.len().saturating_sub(fmt_patterns.len() - 1) {
        for (j, fp) in fmt_patterns.iter().enumerate() {
            if window_start + j >= src_arms.len() {
                continue 'outer;
            }
            let src_t = src_arms[window_start + j].1.trim_start();
            let src_pat = if let Some(pos) = find_outside_string(src_t, " -> ") {
                src_t[..pos].replace(' ', "")
            } else {
                src_t.replace(' ', "")
            };
            if src_pat != *fp {
                continue 'outer;
            }
        }
        // Found matching group. Check alignment of `->` in source.
        let arrow_cols: Vec<usize> = (0..fmt_patterns.len())
            .map(|j| {
                let line = src_arms[window_start + j].1;
                // Find ` -> ` position (first occurrence after `| `)
                find_arrow_col(line)
            })
            .collect();
        // Aligned if all at same column (and column > 0).
        if arrow_cols.iter().all(|c| *c > 0) && arrow_cols.windows(2).all(|w| w[0] == w[1]) {
            return true;
        }
        break;
    }
    false
}

/// Find column (0-based byte offset) of the first ` -> ` outside strings in a match-arm line.
fn find_arrow_col(line: &str) -> usize {
    find_outside_string(line, " -> ").unwrap_or(0)
}

/// Align ` -> ` in lines[start..end] to the maximum column.
fn align_arrows(lines: &mut [String], start: usize, end: usize) {
    let positions: Vec<Option<usize>> = lines[start..end]
        .iter()
        .map(|l| find_outside_string(l, " -> "))
        .collect();
    let max_col = positions.iter().filter_map(|p| *p).max().unwrap_or(0);
    if max_col == 0 {
        return;
    }
    for (i, pos) in positions.iter().enumerate() {
        if let Some(col) = pos {
            if *col < max_col {
                let padding = max_col - col;
                let line = &lines[start + i];
                let (before, after) = line.split_at(*col);
                lines[start + i] = format!("{}{}{}", before, " ".repeat(padding), after);
            }
        }
    }
}

/// Align `=` across groups of consecutive single-line binding entries when
/// the source already had them aligned.
fn align_binding_equals(entries: &mut [FormatEntry], src_lines: &[&str]) {
    let mut i = 0;
    while i < entries.len() {
        if entries[i].kind == EntryKind::Item && is_single_line_let(&entries[i].text) {
            let group_start = i;
            while i < entries.len()
                && entries[i].kind == EntryKind::Item
                && is_single_line_let(&entries[i].text)
            {
                i += 1;
            }
            let group_end = i;
            if group_end - group_start >= 2 {
                // Within this run, find sub-groups with aligned `=` in source.
                align_subgroups(entries, group_start, group_end, src_lines);
            }
        } else {
            i += 1;
        }
    }
}

/// Within entries[start..end], find maximal sub-groups of consecutive entries
/// where source lines had `=` at the same column, and align them.
fn align_subgroups(entries: &mut [FormatEntry], start: usize, end: usize, src_lines: &[&str]) {
    // Get source `=` column for each entry.
    let cols: Vec<Option<usize>> = entries[start..end]
        .iter()
        .map(|e| {
            let idx = e.source_line.saturating_sub(1);
            src_lines.get(idx).and_then(|l| l.find(" = "))
        })
        .collect();

    let mut j = 0;
    while j < cols.len() {
        if let Some(col) = cols[j] {
            let sub_start = j;
            while j < cols.len() && cols[j] == Some(col) {
                j += 1;
            }
            if j - sub_start >= 2 {
                align_equals_in_entries(entries, start + sub_start, start + j);
            }
        } else {
            j += 1;
        }
    }
}

fn is_single_line_let(text: &str) -> bool {
    text.starts_with("let ") && !text.contains('\n')
}

fn align_equals_in_entries(entries: &mut [FormatEntry], start: usize, end: usize) {
    let positions: Vec<Option<usize>> = entries[start..end]
        .iter()
        .map(|e| e.text.find(" = "))
        .collect();
    let max_col = positions.iter().filter_map(|p| *p).max().unwrap_or(0);
    if max_col == 0 {
        return;
    }
    for (i, pos) in positions.iter().enumerate() {
        if let Some(col) = pos {
            if *col < max_col {
                let padding = max_col - col;
                let text = &entries[start + i].text;
                let (before, after) = text.split_at(*col);
                entries[start + i].text =
                    format!("{}{}{}", before, " ".repeat(padding), after);
            }
        }
    }
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
            return nest(ind, concat(hardline(), join(hardline(), arms_doc)));
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
                        nest(ind, concat(hardline(), join(hardline(), arms_doc))),
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
                let lets_block = concat_all(parts);
                return concat_all(vec![
                    text(" "),
                    params_doc,
                    text(" ->"),
                    nest(ind, concat_all(vec![
                        hardline(),
                        lets_block,
                        text(" ".repeat(ind)),
                        fmt_expr(cur, 0),
                    ])),
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

    // Let-in chain: lets at one indent, final body indented one more.
    // e.g. `let aaa =\n  let x = 1 in\n  let y = 2 in\n    z`
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
        parts.push(text(" ".repeat(ind)));
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
        ExprKind::Paren(_) => 100,
        ExprKind::Unary { .. } => 70,
        ExprKind::Apply { .. } => 60,
        _ => 100,
    }
}

fn binop_prec(op: &BinOp) -> u8 {
    match op {
        BinOp::Pipe => 10,
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
                        nest(2, concat(hardline(), join(hardline(), arms_doc))),
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

        ExprKind::Paren(inner) => {
            concat_all(vec![text("("), fmt_expr(inner, 0), text(")")])
        }

        ExprKind::Binary { op, left, right } => {
            let prec = binop_prec(op);
            let op_s = binop_str(op);

            // Pipe chains: collect all chained pipes and format as a unit.
            if matches!(op, BinOp::Pipe) {
                let (base, steps) = collect_pipe_chain(expr);
                let base_doc = fmt_expr(base, prec);
                if steps.len() == 1 {
                    let step_op = binop_str(&steps[0].0);
                    let step_doc = fmt_expr(steps[0].1, prec + 1);
                    return group(concat_all(vec![
                        base_doc,
                        line(),
                        text(format!("{} ", step_op)),
                        step_doc,
                    ]));
                }
                // Multi-step pipe: when broken, each step on its own line.
                let mut parts = vec![base_doc];
                for (step_op, step_expr) in &steps {
                    let sop = binop_str(step_op);
                    parts.push(concat(
                        line(),
                        concat(text(format!("{} ", sop)), fmt_expr(step_expr, prec + 1)),
                    ));
                }
                return group(concat_all(parts));
            }

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
            join(hardline(), arms_doc)
        }

        ExprKind::MatchExpr { scrutinee, arms } => {
            let arms_doc: Vec<Doc> = arms.iter().map(fmt_match_arm).collect();
            concat_all(vec![
                text("match "),
                fmt_expr(scrutinee, 0),
                text(" in"),
                nest(2, concat(hardline(), join(hardline(), arms_doc))),
            ])
        }

        ExprKind::LetIn { .. } => {
            // Flatten let-in chains: lets at one level, body indented.
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
            parts.push(text("  "));
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

/// Collect a left-associative pipe chain: `a |> b |> c` → (a, [(|>, b), (|>, c)])
fn collect_pipe_chain(expr: &Expr) -> (&Expr, Vec<(BinOp, &Expr)>) {
    let mut steps: Vec<(BinOp, &Expr)> = Vec::new();
    let mut cur = expr;
    while let ExprKind::Binary { op, left, right } = &cur.kind {
        if matches!(op, BinOp::Pipe) {
            steps.push((op.clone(), right));
            cur = left;
        } else {
            break;
        }
    }
    steps.reverse();
    (cur, steps)
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
            Some((name, _, _)) => text(format!("..{}", name)),
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
            Some((name, _, _)) => text(format!("..{}", name)),
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
