use crate::builtin::BUILTINS;
use crate::ir;
use crate::codegen::IrModule;
use crate::types::infer::VariantEnv;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// All JS builtins emitted on demand.
/// Each entry: (lume_name, js_name, js_implementation).
/// `_show_internal` must appear before anything that uses it (e.g. `unwrap`, `showRecord`).
static JS_STDLIB: &[(&str, &str, &str)] = &[
    // ── Internal _show (not user-facing; used by unwrap / showRecord) ─────
    ("_show_internal", "_show", concat!(
        "function _show(x) {\n",
        "  if (x === null || x === undefined) return \"{}\";\n",
        "  if (typeof x === \"string\") return x;\n",
        "  if (typeof x === \"number\") return String(x);\n",
        "  if (typeof x === \"boolean\") return x ? \"true\" : \"false\";\n",
        "  if (Array.isArray(x)) return \"[\" + x.map(_show).join(\", \") + \"]\";\n",
        "  if (typeof x === \"object\") {\n",
        "    if (\"$tag\" in x) {\n",
        "      const rest = Object.entries(x).filter(([k]) => k !== \"$tag\");\n",
        "      if (rest.length === 0) return x.$tag;\n",
        "      return x.$tag + \" { \" + rest.map(([k, v]) => k + \": \" + _show(v)).join(\", \") + \" }\";\n",
        "    }\n",
        "    const entries = Object.entries(x);\n",
        "    if (entries.length === 0) return \"{}\";\n",
        "    return \"{ \" + entries.map(([k, v]) => k + \": \" + _show(v)).join(\", \") + \" }\";\n",
        "  }\n",
        "  return String(x);\n",
        "}\n\n",
    )),

    // ── Typed show primitives ─────────────────────────────────────────────
    ("showNum", "showNum", "const showNum = (x) => String(x);\n\n"),
    ("showBool", "showBool", "const showBool = (x) => x ? \"true\" : \"false\";\n\n"),
    ("showText", "showText", "const showText = (x) => x;\n\n"),
    ("showRecord", "showRecord", concat!(
        "function showRecord(x) {\n",
        "  if (x === null || x === undefined) return \"{}\";\n",
        "  if (\"$tag\" in x) {\n",
        "    const rest = Object.entries(x).filter(([k]) => k !== \"$tag\");\n",
        "    if (rest.length === 0) return x.$tag;\n",
        "    return x.$tag + \" { \" + rest.map(([k, v]) => k + \": \" + _show(v)).join(\", \") + \" }\";\n",
        "  }\n",
        "  const entries = Object.entries(x);\n",
        "  if (entries.length === 0) return \"{}\";\n",
        "  return \"{ \" + entries.map(([k, v]) => k + \": \" + _show(v)).join(\", \") + \" }\";\n",
        "}\n\n",
    )),

    // ── I/O ─────────────────────────────────────────────────────────────────
    ("print", "print", "const print = (x) => (console.log(x), {});\n\n"),

    ("mod", "mod", "const mod = (a) => (b) => ((a % b) + b) % b;\n\n"),
    ("pow", "pow", "const pow = (a) => (b) => Math.pow(a, b);\n\n"),
    ("toNum", "toNum", "const toNum = (s) => { const n = Number(s); return isNaN(n) ? { $tag: \"None\" } : { $tag: \"Some\", value: n }; };\n\n"),
    ("range", "range", "const range = (from) => (to) => { const r = []; for (let i = Math.floor(from); i <= Math.floor(to); i++) r.push(i); return r; };\n\n"),

    // Node.js fs-based IO - these will throw at runtime in browser/WASM envs.
    ("readLine", "readLine", concat!(
        "const readLine = (_) => {\n",
        "  const fs = await import(\"fs\"); // sync fallback via readFileSync on /dev/stdin\n",
        "  try { return require(\"readline-sync\").question(\"\"); } catch(_) { return \"\"; }\n",
        "};\n\n",
    )),
    ("readFile", "readFile", concat!(
        "const readFile = (path) => {\n",
        "  try {\n",
        "    const fs = require(\"fs\");\n",
        "    return { $tag: \"Ok\", value: fs.readFileSync(path, \"utf8\") };\n",
        "  } catch(e) { return { $tag: \"Err\", reason: e.message }; }\n",
        "};\n\n",
    )),
    ("writeFile", "writeFile", concat!(
        "const writeFile = (path) => (content) => {\n",
        "  try {\n",
        "    const fs = require(\"fs\");\n",
        "    fs.writeFileSync(path, content, \"utf8\");\n",
        "    return { $tag: \"Ok\", value: {} };\n",
        "  } catch(e) { return { $tag: \"Err\", reason: e.message }; }\n",
        "};\n\n",
    )),
    ("appendFile", "appendFile", concat!(
        "const appendFile = (path) => (content) => {\n",
        "  try {\n",
        "    const fs = require(\"fs\");\n",
        "    fs.appendFileSync(path, content, \"utf8\");\n",
        "    return { $tag: \"Ok\", value: {} };\n",
        "  } catch(e) { return { $tag: \"Err\", reason: e.message }; }\n",
        "};\n\n",
    )),
];

/// Escape JS reserved words to avoid syntax errors in generated code.
fn js_ident(name: &str) -> std::borrow::Cow<'_, str> {
    #[rustfmt::skip]
    const RESERVED: &[&str] = &[
        "break", "case", "catch", "class", "const", "continue", "debugger",
        "default", "delete", "do", "else", "enum", "export", "extends",
        "finally", "for", "function", "if", "implements", "import", "in",
        "instanceof", "interface", "let", "new", "package", "private",
        "protected", "public", "return", "static", "super", "switch", "this",
        "throw", "try", "typeof", "var", "void", "while", "with", "yield",
    ];
    if RESERVED.contains(&name) {
        std::borrow::Cow::Owned(format!("${}", name))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// Check if a name is a valid JS identifier (for field access with dot notation).
fn is_js_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

pub fn emit(bundle: &[IrModule], variant_env: VariantEnv) -> String {
    let module_vars: HashMap<PathBuf, String> = bundle
        .iter()
        .map(|m| (m.canonical.clone(), m.var.clone()))
        .collect();

    let mut e = Emitter {
        out: String::new(),
        needs_result_bind: false,
        needed_stdlib: HashSet::new(),
        module_vars,
        variant_env,
    };

    let last = bundle.len().saturating_sub(1);
    for (i, m) in bundle.iter().enumerate() {
        e.emit_module(&m.module, &m.canonical, &m.var, i == last);
    }

    let mut preamble = String::new();
    if e.needs_result_bind {
        preamble.push_str(
            "function $resultBind(r, f) {\n  return r.$tag === \"Ok\" ? f(r.value) : r;\n}\n\n",
        );
    }
    // Emit stdlib entries in declaration order so dependencies are satisfied.
    // `showRecord` depends on `_show` for nested values.
    if e.needed_stdlib.contains("showRecord") {
        e.needed_stdlib.insert("_show_internal".to_string());
    }
    for (lume_name, _, impl_str) in JS_STDLIB {
        if e.needed_stdlib.contains(*lume_name) {
            preamble.push_str(impl_str);
        }
    }
    for b in BUILTINS {
        if e.needed_stdlib.contains(b.name) {
            preamble.push_str(&format!("const {} = {};\n\n", b.js_name(), b.js));
        }
    }
    if !preamble.is_empty() {
        e.out.insert_str(0, &preamble);
    }
    e.out
}

struct Emitter {
    out: String,
    needs_result_bind: bool,
    needed_stdlib: HashSet<String>,
    module_vars: HashMap<PathBuf, String>,
    variant_env: VariantEnv,
}

impl Emitter {
    fn emit_module(&mut self, module: &ir::Module, canonical: &Path, var: &str, is_entry: bool) {
        if !is_entry {
            self.out.push_str(&format!("const {} = (() => {{\n", var));
        }

        for imp in &module.imports {
            self.emit_import(imp, canonical);
        }
        if !module.imports.is_empty() {
            self.out.push('\n');
        }

        for item in &module.items {
            match item {
                ir::Decl::Let(pat, expr) => {
                    self.emit_binding(pat, expr);
                    self.out.push('\n');
                }
                ir::Decl::LetRec(bindings) => {
                    for (pat, expr) in bindings {
                        self.emit_binding(pat, expr);
                        self.out.push('\n');
                    }
                }
            }
        }

        if is_entry {
            self.out.push_str("\nexport default ");
            self.emit_expr(&module.exports);
            self.out.push_str(";\n");
        } else {
            self.out.push_str("\nreturn ");
            self.emit_expr(&module.exports);
            self.out.push_str(";\n");
            self.out.push_str("})();\n\n");
        }
    }

    fn emit_import(&mut self, imp: &ir::Import, base: &Path) {
        // Try to resolve to a bundled module var first.
        let mod_var = if crate::loader::stdlib_source(&imp.path).is_some() {
            self.module_vars
                .get(&crate::loader::stdlib_path(&imp.path))
                .cloned()
        } else {
            crate::loader::resolve_path(&imp.path, base)
                .ok()
                .and_then(|p| self.module_vars.get(&p).cloned())
        };

        match mod_var {
            Some(mv) => match &imp.binding {
                ir::ImportBinding::Name(name) => {
                    self.out
                        .push_str(&format!("const {} = {};\n", js_ident(name), mv));
                }
                ir::ImportBinding::Destructure(names) => {
                    let names: Vec<_> = names.iter().map(|n| js_ident(n).to_string()).collect();
                    self.out
                        .push_str(&format!("const {{ {} }} = {};\n", names.join(", "), mv));
                }
            },
            None => {
                // Fall back to import statement.
                let raw = &imp.path;
                let path = if raw.ends_with(".lume") {
                    format!("{}.js", &raw[..raw.len() - 5])
                } else {
                    format!("{}.js", raw)
                };
                match &imp.binding {
                    ir::ImportBinding::Name(name) => {
                        self.out
                            .push_str(&format!("import {} from \"{}\";\n", name, path));
                    }
                    ir::ImportBinding::Destructure(names) => {
                        self.out.push_str(&format!(
                            "import {{ {} }} from \"{}\";\n",
                            names.join(", "),
                            path
                        ));
                    }
                }
            }
        }
    }

    fn emit_binding(&mut self, pat: &ir::Pat, value: &ir::Expr) {
        self.out.push_str("const ");
        self.emit_pat_lhs(pat);
        self.out.push_str(" = ");
        self.emit_expr(value);
        self.out.push(';');
    }

    /// Emit a pattern as a JS destructuring LHS (for `const <lhs> = <rhs>`).
    fn emit_pat_lhs(&mut self, p: &ir::Pat) {
        match p {
            ir::Pat::Var(name) => self.out.push_str(&js_ident(name)),
            ir::Pat::Wild => self.out.push_str("_$"),
            ir::Pat::Record { fields, rest } => {
                self.out.push_str("{ ");
                let mut first = true;
                for (name, sub) in fields {
                    if !first {
                        self.out.push_str(", ");
                    }
                    first = false;
                    self.out.push_str(name);
                    if let Some(p) = sub {
                        self.out.push_str(": ");
                        self.emit_pat_lhs(p);
                    }
                }
                if let Some(Some(rest_name)) = rest {
                    if !first {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(&format!("...{}", js_ident(rest_name)));
                }
                self.out.push_str(" }");
            }
            ir::Pat::List { elems, rest } => {
                self.out.push('[');
                let mut first = true;
                for elem in elems {
                    if !first {
                        self.out.push_str(", ");
                    }
                    first = false;
                    self.emit_pat_lhs(elem);
                }
                if let Some(Some(rest_name)) = rest {
                    if !first {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(&format!("...{}", js_ident(rest_name)));
                }
                self.out.push(']');
            }
            _ => self.out.push_str("_$"),
        }
    }

    fn emit_expr(&mut self, expr: &ir::Expr) {
        match expr {
            ir::Expr::Num(n) => self.emit_number(*n),
            ir::Expr::Str(s) => self.emit_string(s),
            ir::Expr::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            ir::Expr::List { bases, elems } => {
                if bases.is_empty() {
                    self.out.push('[');
                    for (i, item) in elems.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.emit_expr(item);
                    }
                    self.out.push(']');
                } else {
                    // Use native JS spread: [...base1, ...base2, elem, ...]
                    self.out.push('[');
                    let mut first = true;
                    for base in bases {
                        if !first { self.out.push_str(", "); }
                        first = false;
                        self.out.push_str("...");
                        self.emit_expr(base);
                    }
                    for e in elems {
                        if !first { self.out.push_str(", "); }
                        first = false;
                        self.emit_expr(e);
                    }
                    self.out.push(']');
                }
            }
            ir::Expr::Var(name) => {
                if JS_STDLIB.iter().any(|(n, _, _)| *n == name.as_str()) {
                    self.needed_stdlib.insert(name.clone());
                } else if let Some(b) = BUILTINS.iter().find(|b| b.name == name.as_str()) {
                    self.needed_stdlib.insert(name.clone());
                    self.out.push_str(&b.js_name());
                    return;
                }
                self.out.push_str(&js_ident(name));
            }
            ir::Expr::Record { bases, fields } => {
                self.out.push_str("{ ");
                let mut first = true;
                for base in bases {
                    if !first {
                        self.out.push_str(", ");
                    }
                    first = false;
                    self.out.push_str("...");
                    self.emit_expr(base);
                }
                for (name, val) in fields {
                    if !first {
                        self.out.push_str(", ");
                    }
                    first = false;
                    if is_js_ident(name) {
                        self.out.push_str(name);
                    } else {
                        self.out.push_str(&format!("\"{}\"", name));
                    }
                    self.out.push_str(": ");
                    self.emit_expr(val);
                }
                self.out.push_str(" }");
            }
            ir::Expr::Field(record, field) => {
                self.emit_access_target(record);
                if is_js_ident(field) {
                    self.out.push('.');
                    self.out.push_str(field);
                } else {
                    self.out.push_str(&format!("[\"{}\"]", field));
                }
            }
            ir::Expr::Tag(name, payload) => match payload {
                None => {
                    self.out.push_str(&format!("{{ $tag: \"{}\" }}", name));
                }
                Some(payload_expr) => {
                    self.out.push_str(&format!("{{ $tag: \"{}\", _0: ", name));
                    self.emit_expr(payload_expr);
                    self.out.push_str(" }");
                }
            },
            ir::Expr::Lam(param, body) => self.emit_lambda(param, body),
            ir::Expr::App(func, arg) => {
                self.emit_call_target(func);
                self.out.push('(');
                self.emit_expr(arg);
                self.out.push(')');
            }
            ir::Expr::BinOp(op, left, right) => self.emit_binary(op, left, right),
            ir::Expr::UnOp(op, operand) => match op {
                ir::UnOp::Neg => {
                    self.out.push('-');
                    self.emit_call_target(operand);
                }
                ir::UnOp::Not => {
                    self.out.push('!');
                    self.emit_call_target(operand);
                }
            },
            ir::Expr::If(cond, then_branch, else_branch) => {
                self.out.push('(');
                self.emit_expr(cond);
                self.out.push_str(" ? ");
                self.emit_expr(then_branch);
                self.out.push_str(" : ");
                self.emit_expr(else_branch);
                self.out.push(')');
            }
            ir::Expr::MatchFn(arms) => self.emit_match_fn(arms),
            ir::Expr::Match(scrutinee, arms) => self.emit_match_expr(scrutinee, arms),
            ir::Expr::Let(pattern, value, body) => {
                // Emit as IIFE: (param => body)(value)
                self.out.push('(');
                self.emit_lambda_param(pattern);
                self.out.push_str(" => ");
                let needs_parens = matches!(**body, ir::Expr::Record { .. });
                if needs_parens {
                    self.out.push('(');
                }
                self.emit_expr(body);
                if needs_parens {
                    self.out.push(')');
                }
                self.out.push_str(")(");
                self.emit_expr(value);
                self.out.push(')');
            }
        }
    }

    fn emit_number(&mut self, n: f64) {
        if n.fract() == 0.0 && n.abs() < 1e15 {
            self.out.push_str(&(n as i64).to_string());
        } else {
            self.out.push_str(&n.to_string());
        }
    }

    fn emit_string(&mut self, s: &str) {
        self.out.push('"');
        for c in s.chars() {
            match c {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }

    /// Parens around complex expressions in field-access position.
    fn emit_access_target(&mut self, expr: &ir::Expr) {
        match expr {
            ir::Expr::Var(_) | ir::Expr::Field(_, _) => self.emit_expr(expr),
            _ => {
                self.out.push('(');
                self.emit_expr(expr);
                self.out.push(')');
            }
        }
    }

    /// Parens around expressions that can't be a callee or unary operand without them.
    fn emit_call_target(&mut self, expr: &ir::Expr) {
        match expr {
            ir::Expr::Var(_)
            | ir::Expr::Field(_, _)
            | ir::Expr::App(_, _)
            | ir::Expr::Num(_)
            | ir::Expr::Str(_)
            | ir::Expr::Bool(_) => self.emit_expr(expr),
            _ => {
                self.out.push('(');
                self.emit_expr(expr);
                self.out.push(')');
            }
        }
    }

    fn emit_lambda(&mut self, param: &ir::Pat, body: &ir::Expr) {
        if Self::is_simple_pattern(param) {
            self.emit_lambda_param(param);
            self.out.push_str(" => ");
            let needs_parens = matches!(body, ir::Expr::Record { .. } | ir::Expr::Tag(_, _));
            if needs_parens {
                self.out.push('(');
            }
            self.emit_expr(body);
            if needs_parens {
                self.out.push(')');
            }
        } else {
            // Refutable pattern: runtime check
            let cond = Self::pattern_cond("$arg", param);
            let binds = self.pattern_binds("$arg", param);
            self.out.push_str("($arg) => {\n  if (");
            self.out.push_str(&cond);
            self.out.push_str(") {\n");
            for (lhs, rhs) in &binds {
                self.out
                    .push_str(&format!("    const {} = {};\n", lhs, rhs));
            }
            self.out.push_str("    return ");
            self.emit_expr(body);
            self.out
                .push_str(";\n  }\n  throw new Error(\"no match\");\n}");
        }
    }

    fn is_simple_pattern(p: &ir::Pat) -> bool {
        match p {
            ir::Pat::Var(_) | ir::Pat::Wild => true,
            ir::Pat::Record { fields, .. } => fields
                .iter()
                .all(|(_, sub)| sub.as_ref().is_none_or(Self::is_simple_pattern)),
            _ => false,
        }
    }

    /// Emit a pattern as an arrow-function parameter (destructuring syntax).
    fn emit_lambda_param(&mut self, p: &ir::Pat) {
        match p {
            ir::Pat::Var(name) => self.out.push_str(&js_ident(name)),
            ir::Pat::Wild => self.out.push('_'),
            ir::Pat::Record { fields, rest } => {
                self.out.push_str("({ ");
                let mut first = true;
                for (name, sub) in fields {
                    if !first {
                        self.out.push_str(", ");
                    }
                    first = false;
                    self.out.push_str(name);
                    if let Some(p) = sub {
                        self.out.push_str(": ");
                        self.emit_lambda_param(p);
                    }
                }
                if let Some(Some(rest_name)) = rest {
                    if !first {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(&format!("...{}", js_ident(rest_name)));
                }
                self.out.push_str(" })");
            }
            _ => self.out.push_str("$arg"),
        }
    }

    fn emit_binary(&mut self, op: &ir::BinOp, left: &ir::Expr, right: &ir::Expr) {
        match op {
            ir::BinOp::Pipe => {
                // x |> f  ==  f(x)
                self.emit_call_target(right);
                self.out.push('(');
                self.emit_expr(left);
                self.out.push(')');
            }
            ir::BinOp::Concat => {
                self.out.push('(');
                self.emit_expr(left);
                self.out.push_str(" + ");
                self.emit_expr(right);
                self.out.push(')');
            }
            other => {
                let js = match other {
                    ir::BinOp::Add => " + ",
                    ir::BinOp::Sub => " - ",
                    ir::BinOp::Mul => " * ",
                    ir::BinOp::Div => " / ",
                    ir::BinOp::Eq => " === ",
                    ir::BinOp::NotEq => " !== ",
                    ir::BinOp::Lt => " < ",
                    ir::BinOp::Gt => " > ",
                    ir::BinOp::LtEq => " <= ",
                    ir::BinOp::GtEq => " >= ",
                    ir::BinOp::And => " && ",
                    ir::BinOp::Or => " || ",
                    _ => unreachable!(),
                };
                self.out.push('(');
                self.emit_expr(left);
                self.out.push_str(js);
                self.emit_expr(right);
                self.out.push(')');
            }
        }
    }

    fn emit_match_fn(&mut self, arms: &[ir::Branch]) {
        self.out.push_str("$v => {\n");
        for arm in arms {
            let cond = Self::pattern_cond("$v", &arm.pattern);
            let binds = self.pattern_binds("$v", &arm.pattern);
            let always_matches = cond == "true";
            let has_guard = arm.guard.is_some();

            self.out.push_str("  {\n");
            for (lhs, rhs) in &binds {
                self.out
                    .push_str(&format!("    const {} = {};\n", lhs, rhs));
            }
            if always_matches && !has_guard {
                self.out.push_str("    return ");
                self.emit_expr(&arm.body);
                self.out.push_str(";\n");
            } else {
                self.out.push_str("    if (");
                if !always_matches {
                    self.out.push_str(&cond);
                }
                if let Some(guard) = &arm.guard {
                    if !always_matches {
                        self.out.push_str(" && ");
                    }
                    self.emit_expr(guard);
                }
                self.out.push_str(") {\n      return ");
                self.emit_expr(&arm.body);
                self.out.push_str(";\n    }\n");
            }
            self.out.push_str("  }\n");
        }
        self.out
            .push_str("  throw new Error(\"incomplete match\");\n}");
    }

    fn emit_match_expr(&mut self, scrutinee: &ir::Expr, arms: &[ir::Branch]) {
        self.out.push_str("(($v) => {\n");
        for arm in arms {
            let cond = Self::pattern_cond("$v", &arm.pattern);
            let binds = self.pattern_binds("$v", &arm.pattern);
            let always_matches = cond == "true";
            let has_guard = arm.guard.is_some();

            self.out.push_str("  {\n");
            for (lhs, rhs) in &binds {
                self.out
                    .push_str(&format!("    const {} = {};\n", lhs, rhs));
            }
            if always_matches && !has_guard {
                self.out.push_str("    return ");
                self.emit_expr(&arm.body);
                self.out.push_str(";\n");
            } else {
                self.out.push_str("    if (");
                if !always_matches {
                    self.out.push_str(&cond);
                }
                if let Some(guard) = &arm.guard {
                    if !always_matches {
                        self.out.push_str(" && ");
                    }
                    self.emit_expr(guard);
                }
                self.out.push_str(") {\n      return ");
                self.emit_expr(&arm.body);
                self.out.push_str(";\n    }\n");
            }
            self.out.push_str("  }\n");
        }
        self.out
            .push_str("  throw new Error(\"incomplete match\");\n})(");
        self.emit_expr(scrutinee);
        self.out.push(')');
    }

    // ── Pattern helpers (pure string computation, no `self.out` mutation) ────────

    /// Returns a JS boolean expression that tests whether `var` matches `pat`.
    fn pattern_cond(var: &str, pat: &ir::Pat) -> String {
        match pat {
            ir::Pat::Wild | ir::Pat::Var(_) => "true".to_string(),
            ir::Pat::Lit(lit) => match lit {
                ir::Lit::Num(n) => {
                    let s = if n.fract() == 0.0 {
                        format!("{}", *n as i64)
                    } else {
                        format!("{}", n)
                    };
                    format!("{} === {}", var, s)
                }
                ir::Lit::Str(s) => format!("{} === \"{}\"", var, s.replace('"', "\\\"")),
                ir::Lit::Bool(b) => format!("{} === {}", var, b),
            },
            ir::Pat::Tag(name, payload) => {
                let tag = format!("{}.$tag === \"{}\"", var, name);
                match payload {
                    None => tag,
                    Some(p) => {
                        let inner = Self::pattern_cond(&format!("{}._0", var), p);
                        if inner == "true" {
                            tag
                        } else {
                            format!("({}) && ({})", tag, inner)
                        }
                    }
                }
            }
            ir::Pat::Record { fields, .. } => {
                let conds: Vec<String> = fields
                    .iter()
                    .filter_map(|(name, sub)| {
                        sub.as_ref().and_then(|p| {
                            let c = Self::pattern_cond(&format!("{}.{}", var, name), p);
                            if c == "true" {
                                None
                            } else {
                                Some(c)
                            }
                        })
                    })
                    .collect();
                if conds.is_empty() {
                    "true".to_string()
                } else {
                    conds.join(" && ")
                }
            }
            ir::Pat::List { elems, rest } => {
                let mut conds = vec![format!("Array.isArray({})", var)];
                if rest.is_none() {
                    conds.push(format!("{}.length === {}", var, elems.len()));
                } else if !elems.is_empty() {
                    conds.push(format!("{}.length >= {}", var, elems.len()));
                }
                for (i, elem) in elems.iter().enumerate() {
                    let c = Self::pattern_cond(&format!("{}[{}]", var, i), elem);
                    if c != "true" {
                        conds.push(c);
                    }
                }
                conds.join(" && ")
            }
        }
    }

    /// Returns `(lhs, rhs)` pairs for `const lhs = rhs` bindings from matching `var` via `pat`.
    fn pattern_binds(&self, var: &str, pat: &ir::Pat) -> Vec<(String, String)> {
        let mut out = Vec::new();
        self.collect_binds(var, pat, &mut out);
        out
    }

    fn collect_binds(&self, var: &str, pat: &ir::Pat, out: &mut Vec<(String, String)>) {
        match pat {
            ir::Pat::Wild | ir::Pat::Lit(_) => {}
            ir::Pat::Var(name) => out.push((js_ident(name).into_owned(), var.to_string())),
            ir::Pat::Tag(name, payload) => {
                if let Some(p) = payload {
                    if self.variant_env.lookup(name).is_some_and(|i| i.wraps.is_some()) {
                        self.collect_binds(&format!("{}._0", var), p, out);
                    } else {
                        self.collect_binds(var, p, out);
                    }
                }
            }
            ir::Pat::Record { fields, rest } => {
                for (name, sub) in fields {
                    let field_expr = format!("{}.{}", var, name);
                    if let Some(p) = sub {
                        self.collect_binds(&field_expr, p, out);
                    } else {
                        out.push((js_ident(name).into_owned(), field_expr));
                    }
                }
                if let Some(Some(rest_name)) = rest {
                    let excluded: Vec<_> = fields
                        .iter()
                        .map(|(name, _)| format!("\"{}\"", name))
                        .collect();
                    out.push((
                        rest_name.clone(),
                        format!(
                            "Object.fromEntries(Object.entries({}).filter(([k]) => ![{}].includes(k)))",
                            var,
                            excluded.join(", ")
                        ),
                    ));
                }
            }
            ir::Pat::List { elems, rest } => {
                for (i, elem) in elems.iter().enumerate() {
                    self.collect_binds(&format!("{}[{}]", var, i), elem, out);
                }
                if let Some(Some(rest_name)) = rest {
                    out.push((
                        js_ident(rest_name).into_owned(),
                        format!("{}.slice({})", var, elems.len()),
                    ));
                }
            }
        }
    }
}
