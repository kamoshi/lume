use crate::ast::*;

/// Escape JS reserved words to avoid syntax errors in generated code.
fn js_ident(name: &str) -> std::borrow::Cow<str> {
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

pub fn emit(program: &Program) -> String {
    let mut e = Emitter { out: String::new(), needs_result_bind: false };
    e.emit_program(program);
    if e.needs_result_bind {
        e.out.insert_str(
            0,
            "function $resultBind(r, f) {\n  return r.$tag === \"Ok\" ? f(r.value) : r;\n}\n\n",
        );
    }
    e.out
}

struct Emitter {
    out: String,
    needs_result_bind: bool,
}

impl Emitter {
    fn emit_program(&mut self, program: &Program) {
        for u in &program.uses {
            self.emit_use(u);
        }
        if !program.uses.is_empty() {
            self.out.push('\n');
        }

        for item in &program.items {
            match item {
                TopItem::TypeDef(_) => {}
                TopItem::Binding(b) => {
                    self.emit_binding(b);
                    self.out.push('\n');
                }
            }
        }

        self.out.push_str("\nexport default ");
        self.emit_expr(&program.exports);
        self.out.push_str(";\n");
    }

    fn emit_use(&mut self, u: &UseDecl) {
        let raw = &u.path;
        let path = if raw.ends_with(".lume") {
            format!("{}.js", &raw[..raw.len() - 5])
        } else {
            format!("{}.js", raw)
        };
        match &u.binding {
            UseBinding::Ident(name) => {
                self.out.push_str(&format!("import {} from \"{}\";\n", name, path));
            }
            UseBinding::Record(rp) => {
                let names: Vec<_> = rp.fields.iter().map(|f| f.name.clone()).collect();
                self.out.push_str(&format!(
                    "import {{ {} }} from \"{}\";\n",
                    names.join(", "),
                    path
                ));
            }
        }
    }

    fn emit_binding(&mut self, b: &Binding) {
        self.out.push_str("const ");
        self.emit_pat_lhs(&b.pattern);
        self.out.push_str(" = ");
        self.emit_expr(&b.value);
        self.out.push(';');
    }

    /// Emit a pattern as a JS destructuring LHS (for `const <lhs> = <rhs>`).
    fn emit_pat_lhs(&mut self, p: &Pattern) {
        match p {
            Pattern::Ident(name) => self.out.push_str(&js_ident(name)),
            Pattern::Wildcard => self.out.push_str("_$"),
            Pattern::Record(rp) => {
                self.out.push_str("{ ");
                let mut first = true;
                for f in &rp.fields {
                    if !first { self.out.push_str(", "); }
                    first = false;
                    self.out.push_str(&f.name);
                    if let Some(p) = &f.pattern {
                        self.out.push_str(": ");
                        self.emit_pat_lhs(p);
                    }
                }
                if let Some(Some(rest_name)) = &rp.rest {
                    if !first { self.out.push_str(", "); }
                    self.out.push_str(&format!("...{}", js_ident(rest_name)));
                }
                self.out.push_str(" }");
            }
            Pattern::List(lp) => {
                self.out.push('[');
                let mut first = true;
                for elem in &lp.elements {
                    if !first { self.out.push_str(", "); }
                    first = false;
                    self.emit_pat_lhs(elem);
                }
                if let Some(Some(rest_name)) = &lp.rest {
                    if !first { self.out.push_str(", "); }
                    self.out.push_str(&format!("...{}", js_ident(rest_name)));
                }
                self.out.push(']');
            }
            _ => self.out.push_str("_$"),
        }
    }

    fn emit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Number(n) => self.emit_number(*n),
            ExprKind::Text(s) => self.emit_string(s),
            ExprKind::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            ExprKind::List(items) => {
                self.out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 { self.out.push_str(", "); }
                    self.emit_expr(item);
                }
                self.out.push(']');
            }
            ExprKind::Ident(name) => self.out.push_str(&js_ident(name)),
            ExprKind::TypeIdent(name) => {
                // Capitalised identifier without payload — unit variant
                self.out.push_str(&format!("{{ $tag: \"{}\" }}", name));
            }
            ExprKind::Record { base, fields, .. } => {
                self.out.push_str("{ ");
                let mut first = true;
                if let Some(base_expr) = base {
                    self.out.push_str("...");
                    self.emit_expr(base_expr);
                    first = false;
                }
                for f in fields {
                    if !first { self.out.push_str(", "); }
                    first = false;
                    self.out.push_str(&f.name);
                    if let Some(val) = &f.value {
                        self.out.push_str(": ");
                        self.emit_expr(val);
                    }
                }
                self.out.push_str(" }");
            }
            ExprKind::FieldAccess { record, field } => {
                self.emit_access_target(record);
                self.out.push('.');
                self.out.push_str(field);
            }
            ExprKind::Variant { name, payload } => match payload {
                None => {
                    self.out.push_str(&format!("{{ $tag: \"{}\" }}", name));
                }
                Some(payload_expr) => {
                    self.out.push_str(&format!("{{ $tag: \"{}\"", name));
                    if let ExprKind::Record { fields, .. } = &payload_expr.kind {
                        for f in fields {
                            self.out.push_str(", ");
                            self.out.push_str(&f.name);
                            if let Some(val) = &f.value {
                                self.out.push_str(": ");
                                self.emit_expr(val);
                            }
                        }
                    }
                    self.out.push_str(" }");
                }
            },
            ExprKind::Lambda { param, body } => self.emit_lambda(param, body),
            ExprKind::Apply { func, arg } => {
                self.emit_call_target(func);
                self.out.push('(');
                self.emit_expr(arg);
                self.out.push(')');
            }
            ExprKind::Binary { op, left, right } => self.emit_binary(op, left, right),
            ExprKind::Unary { op, operand } => match op {
                UnOp::Neg => { self.out.push('-'); self.emit_call_target(operand); }
                UnOp::Not => { self.out.push('!'); self.emit_call_target(operand); }
            },
            ExprKind::If { cond, then_branch, else_branch } => {
                self.out.push('(');
                self.emit_expr(cond);
                self.out.push_str(" ? ");
                self.emit_expr(then_branch);
                self.out.push_str(" : ");
                self.emit_expr(else_branch);
                self.out.push(')');
            }
            ExprKind::Match(arms) => self.emit_match_fn(arms),
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
                '"'  => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                c    => self.out.push(c),
            }
        }
        self.out.push('"');
    }

    /// Parens around complex expressions in field-access position.
    fn emit_access_target(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Ident(_) | ExprKind::FieldAccess { .. } => self.emit_expr(expr),
            _ => { self.out.push('('); self.emit_expr(expr); self.out.push(')'); }
        }
    }

    /// Parens around expressions that can't be a callee or unary operand without them.
    fn emit_call_target(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Ident(_)
            | ExprKind::FieldAccess { .. }
            | ExprKind::Apply { .. }
            | ExprKind::Number(_)
            | ExprKind::Text(_)
            | ExprKind::Bool(_) => self.emit_expr(expr),
            _ => { self.out.push('('); self.emit_expr(expr); self.out.push(')'); }
        }
    }

    fn emit_lambda(&mut self, param: &Pattern, body: &Expr) {
        if Self::is_simple_pattern(param) {
            self.emit_lambda_param(param);
            self.out.push_str(" => ");
            // A record literal as a concise arrow body is ambiguous with a block statement.
            // JS requires parens: `x => ({ ... })` rather than `x => { ... }`.
            let needs_parens = matches!(body.kind, ExprKind::Record { .. });
            if needs_parens { self.out.push('('); }
            self.emit_expr(body);
            if needs_parens { self.out.push(')'); }
        } else {
            // Refutable pattern: runtime check
            let cond = Self::pattern_cond("$arg", param);
            let binds = Self::pattern_binds("$arg", param);
            self.out.push_str("($arg) => {\n  if (");
            self.out.push_str(&cond);
            self.out.push_str(") {\n");
            for (lhs, rhs) in &binds {
                self.out.push_str(&format!("    const {} = {};\n", lhs, rhs));
            }
            self.out.push_str("    return ");
            self.emit_expr(body);
            self.out.push_str(";\n  }\n  throw new Error(\"no match\");\n}");
        }
    }

    fn is_simple_pattern(p: &Pattern) -> bool {
        match p {
            Pattern::Ident(_) | Pattern::Wildcard => true,
            Pattern::Record(rp) => rp.fields.iter().all(|f| {
                f.pattern.as_ref().map_or(true, Self::is_simple_pattern)
            }),
            _ => false,
        }
    }

    /// Emit a pattern as an arrow-function parameter (destructuring syntax).
    fn emit_lambda_param(&mut self, p: &Pattern) {
        match p {
            Pattern::Ident(name) => self.out.push_str(&js_ident(name)),
            Pattern::Wildcard => self.out.push('_'),
            Pattern::Record(rp) => {
                self.out.push_str("({ ");
                let mut first = true;
                for f in &rp.fields {
                    if !first { self.out.push_str(", "); }
                    first = false;
                    self.out.push_str(&f.name);
                    if let Some(p) = &f.pattern {
                        self.out.push_str(": ");
                        self.emit_lambda_param(p);
                    }
                }
                // Open pattern `{ name, .. }` — JS destructuring naturally ignores extras.
                // Named rest `{ name, ..rest }` — emit as spread.
                if let Some(Some(rest_name)) = &rp.rest {
                    if !first { self.out.push_str(", "); }
                    self.out.push_str(&format!("...{}", js_ident(rest_name)));
                }
                self.out.push_str(" })");
            }
            _ => self.out.push_str("$arg"),
        }
    }

    fn emit_binary(&mut self, op: &BinOp, left: &Expr, right: &Expr) {
        match op {
            BinOp::Pipe => {
                // x |> f  ==  f(x)
                self.emit_call_target(right);
                self.out.push('(');
                self.emit_expr(left);
                self.out.push(')');
            }
            BinOp::ResultPipe => {
                self.needs_result_bind = true;
                self.out.push_str("$resultBind(");
                self.emit_expr(left);
                self.out.push_str(", ");
                self.emit_expr(right);
                self.out.push(')');
            }
            BinOp::Concat => {
                self.out.push('(');
                self.emit_expr(left);
                self.out.push_str(" + ");
                self.emit_expr(right);
                self.out.push(')');
            }
            other => {
                let js = match other {
                    BinOp::Add   => " + ",
                    BinOp::Sub   => " - ",
                    BinOp::Mul   => " * ",
                    BinOp::Div   => " / ",
                    BinOp::Eq    => " === ",
                    BinOp::NotEq => " !== ",
                    BinOp::Lt    => " < ",
                    BinOp::Gt    => " > ",
                    BinOp::LtEq  => " <= ",
                    BinOp::GtEq  => " >= ",
                    BinOp::And   => " && ",
                    BinOp::Or    => " || ",
                    _            => unreachable!(),
                };
                self.out.push('(');
                self.emit_expr(left);
                self.out.push_str(js);
                self.emit_expr(right);
                self.out.push(')');
            }
        }
    }

    fn emit_match_fn(&mut self, arms: &[MatchArm]) {
        self.out.push_str("$v => {\n");
        for arm in arms {
            let cond = Self::pattern_cond("$v", &arm.pattern);
            let binds = Self::pattern_binds("$v", &arm.pattern);
            let always_matches = cond == "true";
            let has_guard = arm.guard.is_some();

            // Wrap each arm in a block so `const` bindings don't leak between arms.
            self.out.push_str("  {\n");
            for (lhs, rhs) in &binds {
                self.out.push_str(&format!("    const {} = {};\n", lhs, rhs));
            }
            if always_matches && !has_guard {
                // Unconditional arm — return immediately.
                self.out.push_str("    return ");
                self.emit_expr(&arm.body);
                self.out.push_str(";\n");
            } else {
                // Guarded or refutable arm.
                self.out.push_str("    if (");
                if !always_matches { self.out.push_str(&cond); }
                if let Some(guard) = &arm.guard {
                    if !always_matches { self.out.push_str(" && "); }
                    self.emit_expr(guard);
                }
                self.out.push_str(") {\n      return ");
                self.emit_expr(&arm.body);
                self.out.push_str(";\n    }\n");
            }
            self.out.push_str("  }\n");
        }
        self.out.push_str("  throw new Error(\"incomplete match\");\n}");
    }

    // ── Pattern helpers (pure string computation, no `self.out` mutation) ────────

    /// Returns a JS boolean expression that tests whether `var` matches `pat`.
    fn pattern_cond(var: &str, pat: &Pattern) -> String {
        match pat {
            Pattern::Wildcard | Pattern::Ident(_) => "true".to_string(),
            Pattern::Literal(lit) => match lit {
                Literal::Number(n) => {
                    let s = if n.fract() == 0.0 { format!("{}", *n as i64) } else { format!("{}", n) };
                    format!("{} === {}", var, s)
                }
                Literal::Text(s) => format!("{} === \"{}\"", var, s.replace('"', "\\\"")),
                Literal::Bool(b) => format!("{} === {}", var, b),
            },
            Pattern::Variant { name, payload } => {
                let tag = format!("{}.$tag === \"{}\"", var, name);
                match payload {
                    None => tag,
                    Some(p) => {
                        // Variant fields live on the same object as the tag.
                        let inner = Self::pattern_cond(var, p);
                        if inner == "true" { tag } else { format!("({}) && ({})", tag, inner) }
                    }
                }
            }
            Pattern::Record(rp) => {
                let conds: Vec<String> = rp.fields.iter()
                    .filter_map(|f| f.pattern.as_ref().and_then(|p| {
                        let c = Self::pattern_cond(&format!("{}.{}", var, f.name), p);
                        if c == "true" { None } else { Some(c) }
                    }))
                    .collect();
                if conds.is_empty() { "true".to_string() } else { conds.join(" && ") }
            }
            Pattern::List(lp) => {
                let mut conds = vec![format!("Array.isArray({})", var)];
                if lp.rest.is_none() {
                    conds.push(format!("{}.length === {}", var, lp.elements.len()));
                } else if !lp.elements.is_empty() {
                    conds.push(format!("{}.length >= {}", var, lp.elements.len()));
                }
                for (i, elem) in lp.elements.iter().enumerate() {
                    let c = Self::pattern_cond(&format!("{}[{}]", var, i), elem);
                    if c != "true" { conds.push(c); }
                }
                conds.join(" && ")
            }
        }
    }

    /// Returns `(lhs, rhs)` pairs for `const lhs = rhs` bindings from matching `var` via `pat`.
    fn pattern_binds(var: &str, pat: &Pattern) -> Vec<(String, String)> {
        let mut out = Vec::new();
        Self::collect_binds(var, pat, &mut out);
        out
    }

    fn collect_binds(var: &str, pat: &Pattern, out: &mut Vec<(String, String)>) {
        match pat {
            Pattern::Wildcard | Pattern::Literal(_) => {}
            Pattern::Ident(name) => out.push((js_ident(name).into_owned(), var.to_string())),
            Pattern::Variant { payload, .. } => {
                if let Some(p) = payload {
                    // Variant fields are merged into the tagged object, same var.
                    Self::collect_binds(var, p, out);
                }
            }
            Pattern::Record(rp) => {
                for f in &rp.fields {
                    let field_expr = format!("{}.{}", var, f.name);
                    if let Some(p) = &f.pattern {
                        Self::collect_binds(&field_expr, p, out);
                    } else {
                        out.push((js_ident(&f.name).into_owned(), field_expr));
                    }
                }
                // Named rest via Object.entries filtering
                if let Some(Some(rest_name)) = &rp.rest {
                    let excluded: Vec<_> = rp.fields.iter()
                        .map(|f| format!("\"{}\"", f.name))
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
            Pattern::List(lp) => {
                for (i, elem) in lp.elements.iter().enumerate() {
                    Self::collect_binds(&format!("{}[{}]", var, i), elem, out);
                }
                if let Some(Some(rest_name)) = &lp.rest {
                    out.push((
                        js_ident(rest_name).into_owned(),
                        format!("{}.slice({})", var, lp.elements.len()),
                    ));
                }
            }
        }
    }
}
