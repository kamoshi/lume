use std::path::Path;

use lume_core::ast;
use lume_core::bundle;
use lume_core::codegen;
use lume_core::error::LumeError;
use lume_core::lexer::Lexer;
use lume_core::lower;
use lume_core::parser;
use lume_core::types;

fn parse(src: &str) -> Result<ast::Program, LumeError> {
    let tokens = Lexer::new(src).tokenize()?;
    let program = parser::parse_program(&tokens)?;
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

/// Type-check and lower every module in the bundle to IR.
/// Returns `(Vec<IrModule>, VariantEnv)` on success,
/// or `None` (and prints an error) if any module fails to type-check.
fn lower_bundle(b: &[bundle::BundleModule]) -> Option<(Vec<codegen::IrModule>, types::infer::VariantEnv)> {
    use lume_core::ast::TopItem;

    // Build the global trait/impl/variant context from all modules so cross-module
    // TraitCalls can be resolved and bare constructor references lowered to lambdas.
    let mut global = lower::GlobalCtx {
        traits: std::collections::HashMap::new(),
        impls: std::collections::HashMap::new(),
        param_impls: Vec::new(),
        variants: std::collections::HashMap::new(),
    };
    for m in b.iter() {
        for item in &m.program.items {
            match item {
                TopItem::TraitDef(td) => {
                    global.traits.insert(td.name.clone(), td.clone());
                }
                TopItem::ImplDef(id) => {
                    let dict = lower::dict_name(&id.trait_name, &id.type_name);
                    if id.impl_constraints.is_empty() {
                        global.impls.insert(
                            (id.trait_name.clone(), id.type_name.clone()),
                            lower::ImplEntry {
                                module_var: Some(m.var.clone()),
                                dict_ident: dict,
                            },
                        );
                    } else {
                        global.param_impls.push(lower::ParamImplEntry {
                            trait_name: id.trait_name.clone(),
                            target_type: id.target_type.clone(),
                            constraints: id.impl_constraints.clone(),
                            module_var: Some(m.var.clone()),
                            dict_ident: dict,
                        });
                    }
                }
                TopItem::TypeDef(td) => {
                    for variant in &td.variants {
                        global.variants.insert(
                            variant.name.clone(),
                            lume_core::types::infer::VariantInfo {
                                type_name: td.name.clone(),
                                type_params: td.params.clone(),
                                wraps: variant.wraps.clone(),
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    // Register built-in variants (Maybe, Result) so the lowerer can
    // convert bare constructor references like `Some` and `Ok` into lambdas.
    {
        let mut scratch = lume_core::types::Subst::new();
        let (_, builtin_variants) = types::infer::builtin_env(&mut scratch);
        for (name, info) in builtin_variants.all() {
            global.variants.entry(name.clone()).or_insert_with(|| info.clone());
        }
    }

    // Lower each module with its own local view: impls defined in this
    // module are accessed by bare name (module_var = None).
    let mut ir_modules = Vec::new();
    for m in b.iter() {
        let local_global = lower::GlobalCtx {
            traits: global.traits.clone(),
            impls: global.impls
                .iter()
                .map(|(k, e)| {
                    let is_local = e.module_var.as_deref() == Some(&m.var);
                    let entry = lower::ImplEntry {
                        module_var: if is_local { None } else { e.module_var.clone() },
                        dict_ident: e.dict_ident.clone(),
                    };
                    (k.clone(), entry)
                })
                .collect(),
            param_impls: global.param_impls
                .iter()
                .map(|pi| lower::ParamImplEntry {
                    trait_name: pi.trait_name.clone(),
                    target_type: pi.target_type.clone(),
                    constraints: pi.constraints.clone(),
                    module_var: if pi.module_var.as_deref() == Some(&m.var) {
                        None
                    } else {
                        pi.module_var.clone()
                    },
                    dict_ident: pi.dict_ident.clone(),
                })
                .collect(),
            variants: global.variants.clone(),
        };

        let module_path = Some(m.canonical.as_path());
        let (node_types, type_env) = match types::infer::elaborate_with_env(&m.program, module_path) {
            Ok((nt, env, _)) => (nt, env),
            Err(e) => {
                eprintln!("{}: type error: {e}", m.canonical.display());
                return None;
            }
        };
        let ir_mod = lower::lower(m.program.clone(), &node_types, &type_env, &local_global);
        ir_modules.push(codegen::IrModule {
            canonical: m.canonical.clone(),
            module: ir_mod,
            var: m.var.clone(),
        });
    }

    // Convert the global variants map into a VariantEnv for codegen.
    let mut variant_env = types::infer::VariantEnv::default();
    for (name, info) in global.variants {
        variant_env.insert(name, info);
    }
    Some((ir_modules, variant_env))
}

fn js_file(path: &str) -> bool {
    let b = match bundle::collect(Path::new(path)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{path}: {e}");
            return false;
        }
    };
    let (ir_modules, variant_env) = match lower_bundle(&b) {
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
    let (ir_modules, variant_env) = match lower_bundle(&b) {
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

fn fmt_file(path: &str) -> bool {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{path}: {e}");
            return false;
        }
    };
    // Verify it parses before touching it.
    if let Err(e) = parse(&src) {
        eprintln!("{path}: parse error: {e}");
        return false;
    }
    // Collapse 3+ consecutive newlines into 2 (one blank line max).
    let mut formatted = String::with_capacity(src.len());
    let mut newlines: usize = 0;
    for ch in src.chars() {
        if ch == '\n' {
            newlines += 1;
        } else {
            if newlines > 0 {
                let emit = newlines.min(2);
                for _ in 0..emit {
                    formatted.push('\n');
                }
                newlines = 0;
            }
            formatted.push(ch);
        }
    }
    // Flush trailing newlines, ensure exactly one at EOF.
    formatted.push('\n');

    if formatted == src {
        return true;
    }
    if let Err(e) = std::fs::write(path, &formatted) {
        eprintln!("{path}: {e}");
        return false;
    }
    println!("{path}: formatted");
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
    /// Format source files in place
    Fmt {
        /// Lume source files to format
        #[arg(required = true)]
        files: Vec<String>,
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
        Command::Fmt { files } => run_on_files(&files, fmt_file),
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
        assert!(matches!(toks[1], Token::ResultPipe));
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
        assert!(matches!(parse_expr("[]").kind, ExprKind::List(ref v) if v.is_empty()));
    }

    #[test]
    fn parse_list() {
        assert!(matches!(parse_expr("[1, 2, 3]").kind, ExprKind::List(ref v) if v.len() == 3));
    }

    #[test]
    fn parse_record_literal() {
        let tokens = lex(r#"{ name: "Alice", age: 30 }"#);
        let (_, expr) = parser::parse_expr(&tokens).expect("parse error");
        if let ExprKind::Record { fields, base, .. } = expr.kind {
            assert!(base.is_none());
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "name");
        } else {
            panic!("expected Record");
        }
    }

    #[test]
    fn parse_record_update() {
        let tokens = lex("{ alice | age: 31 }");
        let (_, expr) = parser::parse_expr(&tokens).expect("parse error");
        if let ExprKind::Record { base, fields, .. } = expr.kind {
            assert!(base.is_some());
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "age");
        } else {
            panic!("expected Record update");
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
            assert!(matches!(&lp.rest, Some(Some(s)) if s == "rest"));
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
                ref fields,
                base: None,
                ..
            } if fields.is_empty()
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
            ExprKind::Record { ref fields, .. } if fields.len() == 1
        ));
    }

    #[test]
    fn parse_binding_with_annotation() {
        let tokens = lex("let double : Num -> Num = n -> n * 2");
        let (_, b) = parser::parse_binding(&tokens).expect("parse error");
        assert!(b.ty.is_some());
    }
}
