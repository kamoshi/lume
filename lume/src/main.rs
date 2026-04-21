use std::path::Path;

use lume_core::ast;
use lume_core::bundle;
use lume_core::codegen;
use lume_core::error::LumeError;
use lume_core::lexer::Lexer;
use lume_core::parser;
use lume_core::types;

fn parse(src: &str) -> Result<ast::Program, LumeError> {
    let tokens = Lexer::new(src).tokenize()?;
    let mut program = parser::parse_program(&tokens)?;
    program.pragmas = lume_core::loader::parse_pragmas(src).0;
    Ok(program)
}

fn load(path: &str) -> Option<ast::Program> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{path}: {e}");
            return None;
        }
    };
    match parse(&src) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("{path}: parse error: {e}");
            None
        }
    }
}

fn check_file(path: &str) -> bool {
    let program = match load(path) {
        Some(p) => p,
        None => return false,
    };
    match types::infer::check_program(&program, Some(Path::new(path))) {
        Ok(ty) => {
            println!("{path}: ok  -  exports : {ty}");
            true
        }
        Err(e) => {
            eprintln!("{path}: type error: {e}");
            false
        }
    }
}

/// Type-check, lower, and optimise every module in the bundle.
/// Returns `(Vec<IrModule>, VariantEnv)` on success,
/// or `None` (after printing the error) on the first type error.
fn lower_bundle(mut b: Vec<bundle::BundleModule>) -> Option<(Vec<codegen::IrModule>, types::infer::VariantEnv)> {
    lume_core::pipeline::lower_bundle(&mut b)
        .map_err(|e| eprintln!("{e}"))
        .ok()
}

fn js_file(path: &str) -> bool {
    let b = match bundle::collect(Path::new(path)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{path}: {e}");
            return false;
        }
    };
    let (ir_modules, variant_env) = match lower_bundle(b) {
        Some(v) => v,
        None => return false,
    };
    print!("{}", codegen::js::emit(&ir_modules, variant_env));
    true
}

fn lua_file(path: &str) -> bool {
    let b = match bundle::collect(Path::new(path)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{path}: {e}");
            return false;
        }
    };
    let (ir_modules, variant_env) = match lower_bundle(b) {
        Some(v) => v,
        None => return false,
    };
    print!("{}", codegen::lua::emit(&ir_modules, variant_env));
    true
}

/// Compile to Lua and execute immediately via the vendored LuaJIT runtime.
fn exec_file(path: &str) -> bool {
    match lume_repl::run(path) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("{e}");
            false
        }
    }
}

fn fmt_file(path: &str, to_stdout: bool) -> bool {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{path}: {e}");
            return false;
        }
    };
    let config = lume_core::fmt::FormatConfig::default();
    let formatted = match lume_core::fmt::format_source(&src, &config) {
        Some(f) => f,
        None => {
            eprintln!("{path}: parse error (cannot format)");
            return false;
        }
    };
    if to_stdout {
        print!("{formatted}");
    } else {
        if formatted == src {
            return true;
        }
        if let Err(e) = std::fs::write(path, &formatted) {
            eprintln!("{path}: {e}");
            return false;
        }
        println!("{path}: formatted");
    }
    true
}

fn dump_file(path: &str) -> bool {
    let program = match load(path) {
        Some(p) => p,
        None => return false,
    };
    match types::infer::elaborate_bindings(&program, Some(Path::new(path))) {
        Ok((bindings, exports)) => {
            let width = bindings.iter().map(|b| b.name.len()).max().unwrap_or(0);
            println!("{path}:");
            for b in &bindings {
                println!("  {:<width$} : {}", b.name, b.scheme);
            }
            println!("  {}", "─".repeat(width.max(7) + 3 + 20));
            println!("  {:<width$} : {}", "exports", exports);
            true
        }
        Err(e) => {
            eprintln!("{path}: type error: {e}");
            false
        }
    }
}

use clap::{Parser, Subcommand};

/// The Lume programming language compiler and runtime.
#[derive(Parser)]
#[command(name = "lume", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Type-check source files
    Check {
        /// Lume source files to check
        #[arg(required = true)]
        files: Vec<String>,
    },
    /// Dump inferred types for all bindings
    Dump {
        /// Lume source files to inspect
        #[arg(required = true)]
        files: Vec<String>,
    },
    /// Format source files
    Fmt {
        /// Lume source files to format
        #[arg(required = true)]
        files: Vec<String>,
        /// Print formatted output to stdout instead of writing in place
        #[arg(long)]
        stdout: bool,
    },
    /// Compile to JavaScript and print to stdout
    Js {
        /// Entry-point Lume source file
        #[arg(required = true)]
        file: String,
    },
    /// Compile to Lua and print to stdout
    Lua {
        /// Entry-point Lume source file
        #[arg(required = true)]
        file: String,
    },
    /// Start the Language Server Protocol server
    Lsp,
    /// Start an interactive REPL
    Repl {
        /// Read input from stdin instead of an interactive terminal
        #[arg(long, short = 's')]
        stdin: bool,
        /// Optional Lume file to pre-load into the REPL session
        file: Option<String>,
    },
    /// Compile and execute via the embedded LuaJIT runtime
    Run {
        /// Entry-point Lume source file
        #[arg(required = true)]
        file: String,
    },
}

fn main() {
    let cli = Cli::parse();

    let ok = match cli.command {
        Command::Check { files } => run_on_files(&files, check_file),
        Command::Dump { files } => run_on_files(&files, dump_file),
        Command::Fmt { files, stdout } => {
            let mut ok = true;
            for f in &files {
                if !fmt_file(f, stdout) {
                    ok = false;
                }
            }
            ok
        }
        Command::Js { file } => js_file(&file),
        Command::Lsp => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(lume_lsp::run());
            true
        }
        Command::Lua { file } => lua_file(&file),
        Command::Repl { stdin, file } => {
            if stdin {
                lume_repl::run_repl_stdin(file.as_deref());
            } else {
                lume_repl::run_repl(file.as_deref());
            }
            true
        }
        Command::Run { file } => exec_file(&file),
    };

    if !ok {
        std::process::exit(1);
    }
}

fn run_on_files(files: &[String], f: fn(&str) -> bool) -> bool {
    let mut all_ok = true;
    for path in files {
        if !f(path) {
            all_ok = false;
        }
    }
    all_ok
}

#[cfg(test)]
mod tests {
    use super::parse;
    use lume_core::ast::*;
    use lume_core::lexer::{Lexer, Spanned, Token};
    use lume_core::parser;

    fn lex(src: &str) -> Vec<Spanned> {
        Lexer::new(src).tokenize().expect("lex error")
    }

    // ── Lexer tests ──────────────────────────────────────────────────────────

    #[test]
    fn lex_keywords() {
        let toks: Vec<_> = lex("let pub type use if then else true false")
            .into_iter()
            .map(|s| s.token)
            .collect();
        assert!(matches!(toks[0], Token::Let));
        assert!(matches!(toks[1], Token::Pub));
        assert!(matches!(toks[2], Token::Type));
        assert!(matches!(toks[3], Token::Use));
    }

    #[test]
    fn lex_operators() {
        let toks: Vec<_> = lex("|> ?> -> ++ == != <= >=")
            .into_iter()
            .map(|s| s.token)
            .collect();
        assert!(matches!(toks[0], Token::Pipe));
        assert!(matches!(toks[1], Token::Operator(ref s) if s == "?>"));
        assert!(matches!(toks[2], Token::Arrow));
        assert!(matches!(toks[3], Token::Concat));
        assert!(matches!(toks[4], Token::EqEq));
        assert!(matches!(toks[5], Token::BangEq));
        assert!(matches!(toks[6], Token::LtEq));
        assert!(matches!(toks[7], Token::GtEq));
    }

    #[test]
    fn lex_number() {
        let toks = lex("42 4.20");
        assert!(matches!(toks[0].token, Token::Number(n) if n == 42.0));
        assert!(matches!(toks[1].token, Token::Number(n) if (n - 4.20).abs() < 1e-9));
    }

    #[test]
    fn lex_string() {
        let toks = lex(r#""hello""#);
        assert!(matches!(&toks[0].token, Token::Text(s) if s == "hello"));
    }

    #[test]
    fn lex_comment_ignored() {
        let toks = lex("42 -- this is a comment\n99");
        let nums: Vec<_> = toks
            .iter()
            .filter_map(|t| {
                if let Token::Number(n) = t.token {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(nums, vec![42.0, 99.0]);
    }

    // ── Expression parsing ───────────────────────────────────────────────────

    fn parse_expr(src: &str) -> Expr {
        let tokens = lex(src);
        let (_, expr) = parser::parse_expr(&tokens).expect("parse error");
        expr
    }

    #[test]
    fn parse_number_literal() {
        assert!(matches!(parse_expr("42").kind, ExprKind::Number(n) if n == 42.0));
    }

    #[test]
    fn parse_bool_literals() {
        assert!(matches!(parse_expr("true").kind, ExprKind::Bool(true)));
        assert!(matches!(parse_expr("false").kind, ExprKind::Bool(false)));
    }

    #[test]
    fn parse_text_literal() {
        assert!(matches!(parse_expr(r#""hello""#).kind, ExprKind::Text(ref s) if s == "hello"));
    }

    #[test]
    fn parse_identifier() {
        assert!(matches!(parse_expr("foo").kind, ExprKind::Ident(ref s) if s == "foo"));
    }

    #[test]
    fn parse_field_access() {
        let expr = parse_expr("alice.name");
        assert!(matches!(expr.kind, ExprKind::FieldAccess { ref field, .. } if field == "name"));
    }

    #[test]
    fn parse_binary_add() {
        let expr = parse_expr("1 + 2");
        assert!(matches!(expr.kind, ExprKind::Binary { op: BinOp::Add, .. }));
    }

    #[test]
    fn parse_binary_precedence() {
        // 1 + 2 * 3  should be  1 + (2 * 3)
        let expr = parse_expr("1 + 2 * 3");
        if let ExprKind::Binary { op, right, .. } = expr.kind {
            assert_eq!(op, BinOp::Add);
            assert!(matches!(
                right.kind,
                ExprKind::Binary { op: BinOp::Mul, .. }
            ));
        } else {
            panic!("expected Binary");
        }
    }

    #[test]
    fn parse_pipe() {
        let expr = parse_expr("x |> double");
        assert!(matches!(
            expr.kind,
            ExprKind::Binary {
                op: BinOp::Pipe,
                ..
            }
        ));
    }

    #[test]
    fn parse_concat() {
        let expr = parse_expr(r#""hello" ++ " world""#);
        assert!(matches!(
            expr.kind,
            ExprKind::Binary {
                op: BinOp::Concat,
                ..
            }
        ));
    }

    #[test]
    fn parse_if_expr() {
        let expr = parse_expr("if x > 0 then 1 else 0");
        assert!(matches!(expr.kind, ExprKind::If { .. }));
    }

    #[test]
    fn parse_lambda_simple() {
        let expr = parse_expr("n -> n * 2");
        assert!(matches!(expr.kind, ExprKind::Lambda { .. }));
    }

    #[test]
    fn parse_function_application() {
        let expr = parse_expr("double 5");
        assert!(matches!(expr.kind, ExprKind::Apply { .. }));
    }

    #[test]
    fn parse_function_application_with_record_arg() {
        let expr = parse_expr(r#"double { value: 5 }"#);
        match expr.kind {
            ExprKind::Apply { arg, .. } => {
                assert!(matches!(arg.kind, ExprKind::Record { .. }));
            }
            _ => panic!("expected Apply"),
        }
    }

    #[test]
    fn parse_empty_list() {
        assert!(matches!(parse_expr("[]").kind, ExprKind::List { ref entries } if entries.is_empty()));
    }

    #[test]
    fn parse_list() {
        assert!(matches!(parse_expr("[1, 2, 3]").kind, ExprKind::List { ref entries } if entries.len() == 3));
    }

    #[test]
    fn parse_record_literal() {
        let tokens = lex(r#"{ name: "Alice", age: 30 }"#);
        let (_, expr) = parser::parse_expr(&tokens).expect("parse error");
        if let ExprKind::Record { entries } = &expr.kind {
            let fields: Vec<_> = entries.iter().filter_map(|e| match e {
                RecordEntry::Field(f) => Some(f),
                _ => None,
            }).collect();
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "name");
        } else {
            panic!("expected Record");
        }
    }

    #[test]
    fn parse_record_spread() {
        let tokens = lex("{ ..alice, age: 31 }");
        let (_, expr) = parser::parse_expr(&tokens).expect("parse error");
        if let ExprKind::Record { entries } = &expr.kind {
            let spreads = entries.iter().filter(|e| matches!(e, RecordEntry::Spread(_))).count();
            let fields: Vec<_> = entries.iter().filter_map(|e| match e {
                RecordEntry::Field(f) => Some(f),
                _ => None,
            }).collect();
            assert_eq!(spreads, 1);
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "age");
        } else {
            panic!("expected Record with spread");
        }
    }

    #[test]
    fn parse_variant_unit() {
        let expr = parse_expr("North");
        assert!(
            matches!(expr.kind, ExprKind::Variant { ref name, payload: None } if name == "North")
        );
    }

    #[test]
    fn parse_variant_with_payload() {
        let expr = parse_expr("Circle { radius: 5 }");
        assert!(
            matches!(expr.kind, ExprKind::Variant { ref name, payload: Some(_) } if name == "Circle")
        );
    }

    #[test]
    fn parse_match_expr() {
        let expr = parse_expr("| A -> 1 | B -> 2");
        assert!(matches!(expr.kind, ExprKind::Match(ref arms) if arms.len() == 2));
    }

    #[test]
    fn parse_unary_not() {
        let expr = parse_expr("not true");
        assert!(matches!(expr.kind, ExprKind::Unary { op: UnOp::Not, .. }));
    }

    #[test]
    fn parse_unary_neg() {
        let expr = parse_expr("-5");
        assert!(matches!(expr.kind, ExprKind::Unary { op: UnOp::Neg, .. }));
    }

    // ── Pattern parsing ──────────────────────────────────────────────────────

    fn parse_pattern_str(src: &str) -> Pattern {
        let tokens = lex(src);
        let (_, pat) = parser::parse_pattern(&tokens).expect("parse error");
        pat
    }

    #[test]
    fn pattern_wildcard() {
        assert!(matches!(parse_pattern_str("_"), Pattern::Wildcard));
    }

    #[test]
    fn pattern_ident() {
        assert!(matches!(parse_pattern_str("x"), Pattern::Ident(s, _, _) if s == "x"));
    }

    #[test]
    fn pattern_record_closed() {
        let pat = parse_pattern_str("{ name, age }");
        if let Pattern::Record(rp) = pat {
            assert_eq!(rp.fields.len(), 2);
            assert!(rp.rest.is_none());
        } else {
            panic!("expected Record pattern");
        }
    }

    #[test]
    fn pattern_record_open() {
        let pat = parse_pattern_str("{ name, .. }");
        if let Pattern::Record(rp) = pat {
            assert_eq!(rp.fields.len(), 1);
            assert!(rp.rest.is_some());
        } else {
            panic!("expected open Record pattern");
        }
    }

    #[test]
    fn pattern_list_with_rest() {
        let pat = parse_pattern_str("[x, ..rest]");
        if let Pattern::List(lp) = pat {
            assert_eq!(lp.elements.len(), 1);
            assert!(matches!(&lp.rest, Some(Some((s, _, _))) if s == "rest"));
        } else {
            panic!("expected List pattern");
        }
    }

    #[test]
    fn pattern_variant_unit() {
        let pat = parse_pattern_str("North");
        assert!(matches!(pat, Pattern::Variant { name, payload: None } if name == "North"));
    }

    #[test]
    fn pattern_variant_with_payload() {
        let pat = parse_pattern_str("Circle { radius }");
        assert!(matches!(pat, Pattern::Variant { name, payload: Some(_) } if name == "Circle"));
    }

    // ── Top-level binding ────────────────────────────────────────────────────

    #[test]
    fn parse_let_binding() {
        let tokens = lex("let x = 42");
        let (_, binding) = parser::parse_binding(&tokens[..]).expect("parse error");
        let _ = binding;
    }

    // ── Full program ─────────────────────────────────────────────────────────

    #[test]
    fn parse_minimal_program() {
        let src = "";
        let program = parse(src).expect("should parse");
        assert_eq!(program.uses.len(), 0);
        assert_eq!(program.items.len(), 0);
        assert!(matches!(
            program.exports.kind,
            ExprKind::Record {
                ref entries,
                ..
            } if entries.is_empty()
        ));
    }

    #[test]
    fn parse_use_declaration() {
        let src = r#"use math = "./math""#;
        let program = parse(src).expect("should parse");
        assert_eq!(program.uses.len(), 1);
    }

    #[test]
    fn parse_type_definition() {
        let src = "type Direction = | North | South | East | West\nlet x = 42\npub { x }";
        let program = parse(src).expect("should parse");
        assert_eq!(program.items.len(), 2);
        if let TopItem::TypeDef(td) = &program.items[0] {
            assert_eq!(td.name, "Direction");
            assert_eq!(td.variants.len(), 4);
        }
    }

    #[test]
    fn parse_generic_type() {
        let src = "type Tree a = | Leaf | Node { value: a, left: Tree a, right: Tree a }";
        let program = parse(src).expect("should parse");
        if let TopItem::TypeDef(td) = &program.items[0] {
            assert_eq!(td.params, vec!["a"]);
            assert_eq!(td.variants.len(), 2);
        }
    }

    #[test]
    fn parse_pub_exports() {
        let src = "let x = 42\npub { x }";
        let program = parse(src).expect("should parse");
        assert!(matches!(
            program.exports.kind,
            ExprKind::Record { ref entries, .. } if entries.len() == 1
        ));
    }

    #[test]
    fn parse_binding_with_annotation() {
        let tokens = lex("let double : Num -> Num = n -> n * 2");
        let (_, b) = parser::parse_binding(&tokens).expect("parse error");
        assert!(b.ty.is_some());
    }
}
