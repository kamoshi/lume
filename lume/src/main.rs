use std::path::Path;

use lume::ast;
use lume::bundle;
use lume::codegen;
use lume::desugar;
use lume::error::LumeError;
use lume::lexer::Lexer;
use lume::parser;
use lume::types;

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

fn run_file(path: &str) -> bool {
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

/// Type-check and desugar every module in the bundle in place.
/// Returns `false` (and prints an error) if any module fails to type-check.
fn desugar_bundle(b: &mut Vec<bundle::BundleModule>) -> bool {
    use lume::ast::TopItem;

    // Build the global trait/impl/variant context from all modules so cross-module
    // TraitCalls can be resolved to `_mod_dep.__trait_Type.method` and bare
    // constructor references are desugared to constructor lambdas.
    let mut global = desugar::GlobalCtx {
        traits: std::collections::HashMap::new(),
        impls: std::collections::HashMap::new(),
        variants: std::collections::HashMap::new(),
    };
    for m in b.iter() {
        for item in &m.program.items {
            match item {
                TopItem::TraitDef(td) => {
                    global.traits.insert(td.name.clone(), td.clone());
                }
                TopItem::ImplDef(id) => {
                    let dict = desugar::dict_name(&id.trait_name, &id.type_name);
                    global.impls.insert(
                        (id.trait_name.clone(), id.type_name.clone()),
                        desugar::ImplEntry {
                            // We don't know yet which module is "local"; we'll
                            // patch that per-module below.
                            module_var: Some(m.var.clone()),
                            dict_ident: dict,
                        },
                    );
                }
                TopItem::TypeDef(td) => {
                    for variant in &td.variants {
                        let payload = variant.payload.as_ref().map(|rt| {
                            rt.fields
                                .iter()
                                .map(|f| (f.name.clone(), f.ty.clone()))
                                .collect()
                        });
                        global.variants.insert(
                            variant.name.clone(),
                            lume::types::infer::VariantInfo {
                                type_name: td.name.clone(),
                                type_params: td.params.clone(),
                                payload_fields: payload,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    // Desugar each module with its own local view: impls defined in this
    // module are accessed by bare name (module_var = None).
    for m in b.iter_mut() {
        // Patch local impls to have no module prefix.
        let local_global = desugar::GlobalCtx {
            traits: global.traits.clone(),
            impls: global.impls
                .iter()
                .map(|(k, e)| {
                    let is_local = e.module_var.as_deref() == Some(&m.var);
                    let entry = desugar::ImplEntry {
                        module_var: if is_local { None } else { e.module_var.clone() },
                        dict_ident: e.dict_ident.clone(),
                    };
                    (k.clone(), entry)
                })
                .collect(),
            variants: global.variants.clone(),
        };

        let module_path = Some(m.canonical.as_path());
        let (node_types, type_env) = match types::infer::elaborate_with_env(&m.program, module_path) {
            Ok((nt, env, _)) => (nt, env),
            Err(e) => {
                eprintln!("{}: type error: {e}", m.canonical.display());
                return false;
            }
        };
        m.program = desugar::desugar(m.program.clone(), &node_types, &type_env, &local_global);
        // Suppress the unused-variable warning
        let _ = &local_global;
    }
    true
}

fn js_file(path: &str) -> bool {
    let mut b = match bundle::collect(Path::new(path)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{path}: {e}");
            return false;
        }
    };
    if !desugar_bundle(&mut b) {
        return false;
    }
    print!("{}", codegen::js::emit(&b));
    true
}

fn lua_file(path: &str) -> bool {
    let mut b = match bundle::collect(Path::new(path)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{path}: {e}");
            return false;
        }
    };
    if !desugar_bundle(&mut b) {
        return false;
    }
    print!("{}", codegen::lua::emit(&b));
    true
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let (cmd, paths): (&str, &[String]) = match args.as_slice() {
        [] => {
            eprintln!("Usage: lume [check|dump|fmt|js|lua] <file.lume> ...");
            std::process::exit(1);
        }
        [first, rest @ ..] if first == "check" => ("check", rest),
        [first, rest @ ..] if first == "dump" => ("dump", rest),
        [first, rest @ ..] if first == "fmt" => ("fmt", rest),
        [first, rest @ ..] if first == "js" => ("js", rest),
        [first, rest @ ..] if first == "lua" => ("lua", rest),
        paths => ("check", paths),
    };

    let mut all_ok = true;
    for path in paths {
        let ok = match cmd {
            "dump" => dump_file(path),
            "fmt" => fmt_file(path),
            "js" => js_file(path),
            "lua" => lua_file(path),
            _ => run_file(path),
        };
        if !ok {
            all_ok = false;
        }
    }

    if !all_ok {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use lume::ast::*;
    use lume::lexer::{Lexer, Spanned, Token};
    use lume::parser;

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
