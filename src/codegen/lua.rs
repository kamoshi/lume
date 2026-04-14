use std::collections::HashMap;
use std::path::{Path, PathBuf};
use crate::ast::*;
use crate::bundle::BundleModule;

/// Escape Lua reserved words.
fn lua_ident(name: &str) -> std::borrow::Cow<'_, str> {
    #[rustfmt::skip]
    const RESERVED: &[&str] = &[
        "and", "break", "do", "else", "elseif", "end", "false", "for",
        "function", "goto", "if", "in", "local", "nil", "not", "or",
        "repeat", "return", "then", "true", "until", "while",
    ];
    if RESERVED.contains(&name) {
        std::borrow::Cow::Owned(format!("_{}", name))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

pub fn emit(bundle: &[BundleModule]) -> String {
    let module_vars: HashMap<PathBuf, String> = bundle
        .iter()
        .map(|m| (m.canonical.clone(), m.var.clone()))
        .collect();

    let mut e = Emitter {
        out: String::new(),
        tmp: 0,
        needs_extend: false,
        needs_slice: false,
        needs_omit: false,
        needs_result_bind: false,
        needs_print: false,
        needs_show: false,
        module_vars,
    };

    let last = bundle.len().saturating_sub(1);
    for (i, m) in bundle.iter().enumerate() {
        e.emit_module(&m.program, &m.canonical, &m.var, i == last);
    }

    let mut helpers = String::new();
    if e.needs_result_bind {
        helpers.push_str(
            "local function _resultBind(r, f)\n\
             \x20 if r._tag == \"Ok\" then return f(r.value) else return r end\n\
             end\n\n",
        );
    }
    if e.needs_extend {
        helpers.push_str(
            "local function _extend(t, u)\n\
             \x20 local r = {}\n\
             \x20 for k, v in pairs(t) do r[k] = v end\n\
             \x20 for k, v in pairs(u) do r[k] = v end\n\
             \x20 return r\n\
             end\n\n",
        );
    }
    if e.needs_slice {
        helpers.push_str(
            "local function _slice(t, i)\n\
             \x20 local r = {}\n\
             \x20 for j = i, #t do r[#r + 1] = t[j] end\n\
             \x20 return r\n\
             end\n\n",
        );
    }
    if e.needs_omit {
        helpers.push_str(
            "local function _omit(t, keys)\n\
             \x20 local r = {}\n\
             \x20 local s = {}\n\
             \x20 for _, k in ipairs(keys) do s[k] = true end\n\
             \x20 for k, v in pairs(t) do\n\
             \x20\x20\x20 if not s[k] then r[k] = v end\n\
             \x20 end\n\
             \x20 return r\n\
             end\n\n",
        );
    }
    if e.needs_show {
        helpers.push_str(concat!(
            "local function _show(x)\n",
            "  local t = type(x)\n",
            "  if t == \"string\" then return x end\n",
            "  if t == \"number\" then\n",
            "    if x == math.floor(x) then return tostring(math.floor(x)) else return tostring(x) end\n",
            "  end\n",
            "  if t == \"boolean\" then return x and \"true\" or \"false\" end\n",
            "  if t == \"table\" then\n",
            "    if x._tag ~= nil then\n",
            "      local parts = {}\n",
            "      for k, v in pairs(x) do\n",
            "        if k ~= \"_tag\" then parts[#parts+1] = k .. \": \" .. _show(v) end\n",
            "      end\n",
            "      if #parts == 0 then return x._tag end\n",
            "      return x._tag .. \" { \" .. table.concat(parts, \", \") .. \" }\"\n",
            "    end\n",
            "    local is_list = #x > 0 or next(x) == nil\n",
            "    if is_list and #x > 0 then\n",
            "      local parts = {}\n",
            "      for _, v in ipairs(x) do parts[#parts+1] = _show(v) end\n",
            "      return \"[\" .. table.concat(parts, \", \") .. \"]\"\n",
            "    end\n",
            "    local parts = {}\n",
            "    for k, v in pairs(x) do parts[#parts+1] = k .. \": \" .. _show(v) end\n",
            "    if #parts == 0 then return \"{}\" end\n",
            "    return \"{ \" .. table.concat(parts, \", \") .. \" }\"\n",
            "  end\n",
            "  return tostring(x)\n",
            "end\n\n",
        ));
    }
    if e.needs_print {
        helpers.push_str("local function _print(s) print(s) return {} end\n\n");
    }
    if !helpers.is_empty() {
        e.out.insert_str(0, &helpers);
    }
    e.out
}

struct Emitter {
    out: String,
    tmp: usize,
    needs_extend: bool,
    needs_slice: bool,
    needs_omit: bool,
    needs_result_bind: bool,
    needs_print: bool,
    needs_show: bool,
    /// Canonical path → local variable that holds the module's exports.
    module_vars: HashMap<PathBuf, String>,
}

impl Emitter {
    fn fresh(&mut self) -> String {
        let n = self.tmp;
        self.tmp += 1;
        format!("_t{}", n)
    }

    fn emit_module(&mut self, program: &Program, canonical: &Path, var: &str, is_entry: bool) {
        if !is_entry {
            self.out.push_str(&format!("local {} = (function()\n", var));
        }

        for u in &program.uses {
            self.emit_use(u, canonical);
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

        self.out.push_str("\nreturn ");
        self.emit_expr(&program.exports);
        self.out.push('\n');

        if !is_entry {
            self.out.push_str("end)()\n\n");
        }
    }

    fn emit_use(&mut self, u: &UseDecl, base: &Path) {
        // Try to resolve to a canonical path and look up the bundle var.
        let mod_var = crate::loader::resolve_path(&u.path, base)
            .ok()
            .and_then(|p| self.module_vars.get(&p).cloned());

        match mod_var {
            Some(mv) => match &u.binding {
                UseBinding::Ident(name, _, _) => {
                    self.out
                        .push_str(&format!("local {} = {}\n", lua_ident(name), mv));
                }
                UseBinding::Record(rp) => {
                    for f in &rp.fields {
                        self.out.push_str(&format!(
                            "local {} = {}.{}\n",
                            lua_ident(&f.name),
                            mv,
                            f.name
                        ));
                    }
                }
            },
            None => {
                // Not in bundle — fall back to require().
                let raw = &u.path;
                let path = if raw.ends_with(".lume") {
                    format!("{}.lua", &raw[..raw.len() - 5])
                } else {
                    raw.clone()
                };
                match &u.binding {
                    UseBinding::Ident(name, _, _) => {
                        self.out.push_str(&format!(
                            "local {} = require(\"{}\")\n",
                            lua_ident(name),
                            path
                        ));
                    }
                    UseBinding::Record(rp) => {
                        let tmp = self.fresh();
                        self.out.push_str(&format!(
                            "local {} = require(\"{}\")\n",
                            tmp,
                            path
                        ));
                        for f in &rp.fields {
                            self.out.push_str(&format!(
                                "local {} = {}.{}\n",
                                lua_ident(&f.name),
                                tmp,
                                f.name
                            ));
                        }
                    }
                }
            }
        }
    }

    fn emit_binding(&mut self, b: &Binding) {
        self.emit_pat_binding(&b.pattern, &b.value);
    }

    /// Emit one or more `local` statements for `pat = expr`.
    fn emit_pat_binding(&mut self, pat: &Pattern, expr: &Expr) {
        match pat {
            Pattern::Ident(name, _, _) => {
                self.out.push_str(&format!("local {} = ", lua_ident(name)));
                self.emit_expr(expr);
            }
            Pattern::Wildcard => {
                self.out.push_str("local _ = ");
                self.emit_expr(expr);
            }
            Pattern::Record(rp) => {
                let tmp = self.fresh();
                self.out.push_str(&format!("local {} = ", tmp));
                self.emit_expr(expr);
                self.out.push('\n');
                for f in &rp.fields {
                    let src = format!("{}.{}", tmp, f.name);
                    if let Some(p) = &f.pattern {
                        let binds = self.collect_binds_pure(&src, p);
                        for (lhs, rhs) in binds {
                            self.out.push_str(&format!("local {} = {}\n", lhs, rhs));
                        }
                    } else {
                        self.out
                            .push_str(&format!("local {} = {}\n", lua_ident(&f.name), src));
                    }
                }
                if let Some(Some(rest_name)) = &rp.rest {
                    self.needs_omit = true;
                    let excluded: Vec<_> = rp
                        .fields
                        .iter()
                        .map(|f| format!("\"{}\"", f.name))
                        .collect();
                    self.out.push_str(&format!(
                        "local {} = _omit({}, {{{}}})\n",
                        rest_name,
                        tmp,
                        excluded.join(", ")
                    ));
                }
            }
            Pattern::List(lp) => {
                let tmp = self.fresh();
                self.out.push_str(&format!("local {} = ", tmp));
                self.emit_expr(expr);
                self.out.push('\n');
                for (i, elem) in lp.elements.iter().enumerate() {
                    let src = format!("{}[{}]", tmp, i + 1);
                    let binds = self.collect_binds_pure(&src, elem);
                    for (lhs, rhs) in binds {
                        self.out.push_str(&format!("local {} = {}\n", lhs, rhs));
                    }
                }
                if let Some(Some(rest_name)) = &lp.rest {
                    self.needs_slice = true;
                    self.out.push_str(&format!(
                        "local {} = _slice({}, {})\n",
                        rest_name,
                        tmp,
                        lp.elements.len() + 1
                    ));
                }
            }
            _ => {
                self.out.push_str("local _ = ");
                self.emit_expr(expr);
            }
        }
    }

    fn emit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Number(n) => self.emit_number(*n),
            ExprKind::Text(s) => self.emit_string(s),
            ExprKind::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            ExprKind::List(items) => {
                self.out.push('{');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.emit_expr(item);
                }
                self.out.push('}');
            }
            ExprKind::Ident(name) => {
                if name == "print" {
                    self.needs_print = true;
                    self.out.push_str("_print");
                } else if name == "show" {
                    self.needs_show = true;
                    self.out.push_str("_show");
                } else {
                    self.out.push_str(&lua_ident(name));
                }
            }
            ExprKind::Record { base, fields, .. } => {
                if let Some(base_expr) = base {
                    // Record update: _extend(base, { overrides })
                    self.needs_extend = true;
                    self.out.push_str("_extend(");
                    self.emit_expr(base_expr);
                    self.out.push_str(", {");
                    for (i, f) in fields.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.out.push_str(&format!("{} = ", f.name));
                        if let Some(val) = &f.value {
                            self.emit_expr(val);
                        } else {
                            self.out.push_str(&lua_ident(&f.name));
                        }
                    }
                    self.out.push_str("})");
                } else {
                    self.out.push('{');
                    for (i, f) in fields.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.out.push_str(&format!("{} = ", f.name));
                        if let Some(val) = &f.value {
                            self.emit_expr(val);
                        } else {
                            self.out.push_str(&lua_ident(&f.name));
                        }
                    }
                    self.out.push('}');
                }
            }
            ExprKind::FieldAccess { record, field } => {
                self.emit_access_target(record);
                self.out.push('.');
                self.out.push_str(field);
            }
            ExprKind::Variant { name, payload } => match payload {
                None => {
                    self.out.push_str(&format!("{{_tag = \"{}\"}}", name));
                }
                Some(payload_expr) => {
                    self.out.push_str(&format!("{{_tag = \"{}\"", name));
                    if let ExprKind::Record { fields, .. } = &payload_expr.kind {
                        for f in fields {
                            self.out.push_str(&format!(", {} = ", f.name));
                            if let Some(val) = &f.value {
                                self.emit_expr(val);
                            } else {
                                self.out.push_str(&lua_ident(&f.name));
                            }
                        }
                    }
                    self.out.push('}');
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
                UnOp::Neg => {
                    self.out.push('-');
                    self.emit_call_target(operand);
                }
                UnOp::Not => {
                    self.out.push_str("not ");
                    self.emit_call_target(operand);
                }
            },
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.out.push_str("(function() if ");
                self.emit_expr(cond);
                self.out.push_str(" then return ");
                self.emit_expr(then_branch);
                self.out.push_str(" else return ");
                self.emit_expr(else_branch);
                self.out.push_str(" end end)()");
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
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\t' => self.out.push_str("\\t"),
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }

    fn emit_access_target(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Ident(_) | ExprKind::FieldAccess { .. } => self.emit_expr(expr),
            _ => {
                self.out.push('(');
                self.emit_expr(expr);
                self.out.push(')');
            }
        }
    }

    fn emit_call_target(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Ident(_)
            | ExprKind::FieldAccess { .. }
            | ExprKind::Apply { .. }
            | ExprKind::Number(_)
            | ExprKind::Text(_)
            | ExprKind::Bool(_) => self.emit_expr(expr),
            _ => {
                self.out.push('(');
                self.emit_expr(expr);
                self.out.push(')');
            }
        }
    }

    fn emit_lambda(&mut self, param: &Pattern, body: &Expr) {
        match param {
            Pattern::Ident(name, _, _) => {
                self.out
                    .push_str(&format!("function({}) return ", lua_ident(name)));
                self.emit_expr(body);
                self.out.push_str(" end");
            }
            Pattern::Wildcard => {
                self.out.push_str("function(_) return ");
                self.emit_expr(body);
                self.out.push_str(" end");
            }
            Pattern::Record(rp) => {
                self.out.push_str("function(_arg)\n");
                for f in &rp.fields {
                    if let Some(p) = &f.pattern {
                        let binds = self.collect_binds_pure(&format!("_arg.{}", f.name), p);
                        for (lhs, rhs) in binds {
                            self.out.push_str(&format!("  local {} = {}\n", lhs, rhs));
                        }
                    } else {
                        self.out.push_str(&format!(
                            "  local {} = _arg.{}\n",
                            lua_ident(&f.name),
                            f.name
                        ));
                    }
                }
                if let Some(Some(rest_name)) = &rp.rest {
                    self.needs_omit = true;
                    let excluded: Vec<_> = rp
                        .fields
                        .iter()
                        .map(|f| format!("\"{}\"", f.name))
                        .collect();
                    self.out.push_str(&format!(
                        "  local {} = _omit(_arg, {{{}}})\n",
                        rest_name,
                        excluded.join(", ")
                    ));
                }
                self.out.push_str("  return ");
                self.emit_expr(body);
                self.out.push_str("\nend");
            }
            _ => {
                // Refutable pattern: runtime check
                let cond = Self::pattern_cond("_arg", param);
                let binds = self.collect_binds_pure("_arg", param);
                self.out.push_str("function(_arg)\n");
                for (lhs, rhs) in &binds {
                    self.out.push_str(&format!("  local {} = {}\n", lhs, rhs));
                }
                if cond == "true" {
                    self.out.push_str("  return ");
                    self.emit_expr(body);
                    self.out.push_str("\nend");
                } else {
                    self.out
                        .push_str(&format!("  if {} then\n    return ", cond));
                    self.emit_expr(body);
                    self.out.push_str("\n  end\n  error(\"no match\")\nend");
                }
            }
        }
    }

    fn emit_binary(&mut self, op: &BinOp, left: &Expr, right: &Expr) {
        match op {
            BinOp::Pipe => {
                self.emit_call_target(right);
                self.out.push('(');
                self.emit_expr(left);
                self.out.push(')');
            }
            BinOp::ResultPipe => {
                self.needs_result_bind = true;
                self.out.push_str("_resultBind(");
                self.emit_expr(left);
                self.out.push_str(", ");
                self.emit_expr(right);
                self.out.push(')');
            }
            BinOp::Concat => {
                self.out.push('(');
                self.emit_expr(left);
                self.out.push_str(" .. ");
                self.emit_expr(right);
                self.out.push(')');
            }
            other => {
                let lua = match other {
                    BinOp::Add => " + ",
                    BinOp::Sub => " - ",
                    BinOp::Mul => " * ",
                    BinOp::Div => " / ",
                    BinOp::Eq => " == ",
                    BinOp::NotEq => " ~= ",
                    BinOp::Lt => " < ",
                    BinOp::Gt => " > ",
                    BinOp::LtEq => " <= ",
                    BinOp::GtEq => " >= ",
                    BinOp::And => " and ",
                    BinOp::Or => " or ",
                    _ => unreachable!(),
                };
                self.out.push('(');
                self.emit_expr(left);
                self.out.push_str(lua);
                self.emit_expr(right);
                self.out.push(')');
            }
        }
    }

    fn emit_match_fn(&mut self, arms: &[MatchArm]) {
        self.out.push_str("function(_v)\n");
        for arm in arms {
            let cond = Self::pattern_cond("_v", &arm.pattern);
            let binds = self.collect_binds_pure("_v", &arm.pattern);
            let always_matches = cond == "true";
            let has_guard = arm.guard.is_some();

            self.out.push_str("  do\n");
            for (lhs, rhs) in &binds {
                self.out.push_str(&format!("    local {} = {}\n", lhs, rhs));
            }
            if always_matches && !has_guard {
                self.out.push_str("    return ");
                self.emit_expr(&arm.body);
                self.out.push('\n');
            } else {
                self.out.push_str("    if ");
                if !always_matches {
                    self.out.push_str(&cond);
                }
                if let Some(guard) = &arm.guard {
                    if !always_matches {
                        self.out.push_str(" and ");
                    }
                    self.emit_expr(guard);
                }
                self.out.push_str(" then\n      return ");
                self.emit_expr(&arm.body);
                self.out.push_str("\n    end\n");
            }
            self.out.push_str("  end\n");
        }
        self.out.push_str("  error(\"incomplete match\")\nend");
    }

    // ── Pattern helpers ───────────────────────────────────────────────────────

    /// Returns a Lua boolean expression testing whether `var` matches `pat`.
    fn pattern_cond(var: &str, pat: &Pattern) -> String {
        match pat {
            Pattern::Wildcard | Pattern::Ident(_, _, _) => "true".to_string(),
            Pattern::Literal(lit) => match lit {
                Literal::Number(n) => {
                    let s = if n.fract() == 0.0 {
                        format!("{}", *n as i64)
                    } else {
                        format!("{}", n)
                    };
                    format!("{} == {}", var, s)
                }
                Literal::Text(s) => {
                    format!("{} == \"{}\"", var, s.replace('"', "\\\""))
                }
                Literal::Bool(b) => format!("{} == {}", var, b),
            },
            Pattern::Variant { name, payload } => {
                let tag = format!("{}._tag == \"{}\"", var, name);
                match payload {
                    None => tag,
                    Some(p) => {
                        let inner = Self::pattern_cond(var, p);
                        if inner == "true" {
                            tag
                        } else {
                            format!("({}) and ({})", tag, inner)
                        }
                    }
                }
            }
            Pattern::Record(rp) => {
                let conds: Vec<String> = rp
                    .fields
                    .iter()
                    .filter_map(|f| {
                        f.pattern.as_ref().and_then(|p| {
                            let c = Self::pattern_cond(&format!("{}.{}", var, f.name), p);
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
                    conds.join(" and ")
                }
            }
            Pattern::List(lp) => {
                let mut conds = vec![format!("type({}) == \"table\"", var)];
                if lp.rest.is_none() {
                    conds.push(format!("#{} == {}", var, lp.elements.len()));
                } else if !lp.elements.is_empty() {
                    conds.push(format!("#{} >= {}", var, lp.elements.len()));
                }
                for (i, elem) in lp.elements.iter().enumerate() {
                    let c = Self::pattern_cond(&format!("{}[{}]", var, i + 1), elem);
                    if c != "true" {
                        conds.push(c);
                    }
                }
                conds.join(" and ")
            }
        }
    }

    /// Returns `(lhs, rhs)` binding pairs from matching `var` against `pat`.
    /// Sets helper flags on `self` when `_slice` / `_omit` are needed.
    fn collect_binds_pure(&mut self, var: &str, pat: &Pattern) -> Vec<(String, String)> {
        let mut out = Vec::new();
        self.collect_binds(var, pat, &mut out);
        out
    }

    fn collect_binds(&mut self, var: &str, pat: &Pattern, out: &mut Vec<(String, String)>) {
        match pat {
            Pattern::Wildcard | Pattern::Literal(_) => {}
            Pattern::Ident(name, _, _) => out.push((lua_ident(name).into_owned(), var.to_string())),
            Pattern::Variant { payload, .. } => {
                if let Some(p) = payload {
                    self.collect_binds(var, p, out);
                }
            }
            Pattern::Record(rp) => {
                for f in &rp.fields {
                    let field_expr = format!("{}.{}", var, f.name);
                    if let Some(p) = &f.pattern {
                        self.collect_binds(&field_expr, p, out);
                    } else {
                        out.push((lua_ident(&f.name).into_owned(), field_expr));
                    }
                }
                if let Some(Some(rest_name)) = &rp.rest {
                    self.needs_omit = true;
                    let excluded: Vec<_> = rp
                        .fields
                        .iter()
                        .map(|f| format!("\"{}\"", f.name))
                        .collect();
                    out.push((
                        rest_name.clone(),
                        format!("_omit({}, {{{}}})", var, excluded.join(", ")),
                    ));
                }
            }
            Pattern::List(lp) => {
                for (i, elem) in lp.elements.iter().enumerate() {
                    self.collect_binds(&format!("{}[{}]", var, i + 1), elem, out);
                }
                if let Some(Some(rest_name)) = &lp.rest {
                    self.needs_slice = true;
                    out.push((
                        lua_ident(rest_name).into_owned(),
                        format!("_slice({}, {})", var, lp.elements.len() + 1),
                    ));
                }
            }
        }
    }
}
