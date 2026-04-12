mod ast;
mod codegen;
mod error;
mod lexer;
mod parser;
mod types;

use error::LumeError;
use lexer::Lexer;

fn parse(src: &str) -> Result<ast::Program, LumeError> {
    let tokens = Lexer::new(src).tokenize()?;
    let program = parser::parse_program(&tokens)?;
    Ok(program)
}

fn typecheck(src: &str) -> Result<types::Ty, LumeError> {
    let program = parse(src)?;
    let ty = types::infer::check_program(&program)?;
    Ok(ty)
}

fn load(path: &str) -> Option<ast::Program> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => { eprintln!("{path}: {e}"); return None; }
    };
    match parse(&src) {
        Ok(p) => Some(p),
        Err(e) => { eprintln!("{path}: parse error: {e}"); None }
    }
}

fn run_file(path: &str) -> bool {
    let program = match load(path) { Some(p) => p, None => return false };
    match types::infer::check_program(&program) {
        Ok(ty) => { println!("{path}: ok  —  exports : {ty}"); true }
        Err(e) => { eprintln!("{path}: type error: {e}"); false }
    }
}

fn js_file(path: &str) -> bool {
    let program = match load(path) { Some(p) => p, None => return false };
    // Typecheck first so we only emit valid programs
    match types::infer::check_program(&program) {
        Ok(_) => {}
        Err(e) => { eprintln!("{path}: type error: {e}"); return false; }
    }
    print!("{}", codegen::emit(&program));
    true
}

fn dump_file(path: &str) -> bool {
    let program = match load(path) { Some(p) => p, None => return false };
    match types::infer::elaborate(&program) {
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
        Err(e) => { eprintln!("{path}: type error: {e}"); false }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let (cmd, paths): (&str, &[String]) = match args.as_slice() {
        [] => {
            eprintln!("Usage: lume [check|dump|js] <file.lume> ...");
            std::process::exit(1);
        }
        [first, rest @ ..] if first == "dump" => ("dump", rest),
        [first, rest @ ..] if first == "js"   => ("js",   rest),
        paths => ("check", paths),
    };

    let mut all_ok = true;
    for path in paths {
        let ok = match cmd {
            "dump" => dump_file(path),
            "js"   => js_file(path),
            _      => run_file(path),
        };
        if !ok { all_ok = false; }
    }

    if !all_ok { std::process::exit(1); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;
    use crate::lexer::Lexer;
    use crate::parser;

    fn lex(src: &str) -> Vec<crate::lexer::Spanned> {
        Lexer::new(src).tokenize().expect("lex error")
    }

    // ── Lexer tests ──────────────────────────────────────────────────────────

    #[test]
    fn lex_keywords() {
        use crate::lexer::Token;
        let toks: Vec<_> = lex("let type use if then else true false")
            .into_iter()
            .map(|s| s.token)
            .collect();
        assert!(matches!(toks[0], Token::Let));
        assert!(matches!(toks[1], Token::Type));
        assert!(matches!(toks[2], Token::Use));
    }

    #[test]
    fn lex_operators() {
        use crate::lexer::Token;
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
        use crate::lexer::Token;
        let toks = lex("42 3.14");
        assert!(matches!(toks[0].token, Token::Number(n) if n == 42.0));
        assert!(matches!(toks[1].token, Token::Number(n) if (n - 3.14).abs() < 1e-9));
    }

    #[test]
    fn lex_string() {
        use crate::lexer::Token;
        let toks = lex(r#""hello""#);
        assert!(matches!(&toks[0].token, Token::Text(s) if s == "hello"));
    }

    #[test]
    fn lex_comment_ignored() {
        use crate::lexer::Token;
        let toks = lex("42 -- this is a comment\n99");
        let nums: Vec<_> = toks.iter()
            .filter_map(|t| if let Token::Number(n) = t.token { Some(n) } else { None })
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
        assert!(matches!(parse_expr("42"), Expr::Number(n) if n == 42.0));
    }

    #[test]
    fn parse_bool_literals() {
        assert!(matches!(parse_expr("true"), Expr::Bool(true)));
        assert!(matches!(parse_expr("false"), Expr::Bool(false)));
    }

    #[test]
    fn parse_text_literal() {
        assert!(matches!(parse_expr(r#""hello""#), Expr::Text(s) if s == "hello"));
    }

    #[test]
    fn parse_identifier() {
        assert!(matches!(parse_expr("foo"), Expr::Ident(s) if s == "foo"));
    }

    #[test]
    fn parse_field_access() {
        let expr = parse_expr("alice.name");
        assert!(matches!(expr, Expr::FieldAccess { field, .. } if field == "name"));
    }

    #[test]
    fn parse_binary_add() {
        let expr = parse_expr("1 + 2");
        assert!(matches!(expr, Expr::Binary { op: BinOp::Add, .. }));
    }

    #[test]
    fn parse_binary_precedence() {
        // 1 + 2 * 3  should be  1 + (2 * 3)
        let expr = parse_expr("1 + 2 * 3");
        if let Expr::Binary { op, right, .. } = expr {
            assert_eq!(op, BinOp::Add);
            assert!(matches!(*right, Expr::Binary { op: BinOp::Mul, .. }));
        } else {
            panic!("expected Binary");
        }
    }

    #[test]
    fn parse_pipe() {
        let expr = parse_expr("x |> double");
        assert!(matches!(expr, Expr::Binary { op: BinOp::Pipe, .. }));
    }

    #[test]
    fn parse_concat() {
        let expr = parse_expr(r#""hello" ++ " world""#);
        assert!(matches!(expr, Expr::Binary { op: BinOp::Concat, .. }));
    }

    #[test]
    fn parse_if_expr() {
        let expr = parse_expr("if x > 0 then 1 else 0");
        assert!(matches!(expr, Expr::If { .. }));
    }

    #[test]
    fn parse_lambda_simple() {
        let expr = parse_expr("n -> n * 2");
        assert!(matches!(expr, Expr::Lambda { .. }));
    }

    #[test]
    fn parse_function_application() {
        let expr = parse_expr("double 5");
        assert!(matches!(expr, Expr::Apply { .. }));
    }

    #[test]
    fn parse_empty_list() {
        assert!(matches!(parse_expr("[]"), Expr::List(v) if v.is_empty()));
    }

    #[test]
    fn parse_list() {
        assert!(matches!(parse_expr("[1, 2, 3]"), Expr::List(v) if v.len() == 3));
    }

    #[test]
    fn parse_record_literal() {
        // Records are parsed as standalone atoms; as arguments they need parens.
        let tokens = lex(r#"{ name: "Alice", age: 30 }"#);
        let (_, expr) = parser::parse_expr(&tokens).expect("parse error");
        if let Expr::Record { fields, base, .. } = expr {
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
        if let Expr::Record { base, fields, .. } = expr {
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
        assert!(matches!(expr, Expr::Variant { name, payload: None } if name == "North"));
    }

    #[test]
    fn parse_variant_with_payload() {
        let expr = parse_expr("Circle { radius: 5 }");
        assert!(matches!(expr, Expr::Variant { name, payload: Some(_) } if name == "Circle"));
    }

    #[test]
    fn parse_match_expr() {
        let expr = parse_expr("| A -> 1 | B -> 2");
        assert!(matches!(expr, Expr::Match(arms) if arms.len() == 2));
    }

    #[test]
    fn parse_unary_not() {
        let expr = parse_expr("not true");
        assert!(matches!(expr, Expr::Unary { op: UnOp::Not, .. }));
    }

    #[test]
    fn parse_unary_neg() {
        let expr = parse_expr("-5");
        assert!(matches!(expr, Expr::Unary { op: UnOp::Neg, .. }));
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
        assert!(matches!(parse_pattern_str("x"), Pattern::Ident(s) if s == "x"));
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
        let (_, binding) = crate::parser::parse_binding(&tokens[..]).expect("parse error");
        // Note: parse_binding is not pub — we test through parse_program
        let _ = binding;
    }

    // ── Full program ─────────────────────────────────────────────────────────

    #[test]
    fn parse_minimal_program() {
        let src = "{}";
        let program = parse(src).expect("should parse");
        assert_eq!(program.uses.len(), 0);
        assert_eq!(program.items.len(), 0);
    }

    #[test]
    fn parse_use_declaration() {
        let src = r#"use math = "./math" {}"#;
        let program = parse(src).expect("should parse");
        assert_eq!(program.uses.len(), 1);
    }

    #[test]
    fn parse_type_definition() {
        // The `let` binding after the type def terminates the variant list,
        // which prevents the export record `{ x }` from being greedily consumed
        // as the last variant's payload — the same shape as realistic Lume modules.
        let src = "type Direction = | North | South | East | West\nlet x = 42\n{ x }";
        let program = parse(src).expect("should parse");
        // items = [TypeDef(Direction), Binding(x)]
        assert_eq!(program.items.len(), 2);
        if let TopItem::TypeDef(td) = &program.items[0] {
            assert_eq!(td.name, "Direction");
            assert_eq!(td.variants.len(), 4);
        }
    }

    #[test]
    fn parse_generic_type() {
        let src = "type Tree a = | Leaf | Node { value: a, left: Tree a, right: Tree a } {}";
        let program = parse(src).expect("should parse");
        if let TopItem::TypeDef(td) = &program.items[0] {
            assert_eq!(td.params, vec!["a"]);
            assert_eq!(td.variants.len(), 2);
        }
    }

    #[test]
    fn parse_binding_with_annotation() {
        // Parse the binding directly to avoid the trailing-export ambiguity.
        let tokens = lex("let double : Num -> Num = n -> n * 2");
        let (_, b) = parser::parse_binding(&tokens).expect("parse error");
        assert!(b.ty.is_some());
    }
}
