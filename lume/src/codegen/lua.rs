use crate::ast::*;
use crate::bundle::BundleModule;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// All user-visible stdlib functions.
///
/// Order is significant: functions that appear earlier are emitted first, so
/// dependencies (e.g. `_show` needed by `unwrap`) are satisfied.
///
/// Each entry is `(lume_name, lua_name, lua_implementation)`.
static STDLIB: &[(&str, &str, &str)] = &[
    // ── Reflection (must come first; used by `unwrap`) ─────────────────────
    ("show", "_show", concat!(
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
    )),

    // ── I/O ────────────────────────────────────────────────────────────────
    // `print` is renamed to `_print` so it returns `{}` instead of nil and
    // does not shadow the raw Lua `print` used inside `_show` above.
    ("print", "_print", "local function _print(s) print(s) return {} end\n\n"),

    // readLine : {} -> Text  - reads one line from stdin (strips trailing newline)
    ("readLine", "readLine", "local readLine = function(_) return io.read(\"l\") or \"\" end\n\n"),

    // readFile : Text -> Result Text Text
    ("readFile", "readFile", concat!(
        "local readFile = function(path)\n",
        "  local f, err = io.open(path, \"r\")\n",
        "  if not f then return {_tag=\"Err\", reason=err} end\n",
        "  local content = f:read(\"*a\")\n",
        "  f:close()\n",
        "  return {_tag=\"Ok\", value=content}\n",
        "end\n\n",
    )),

    // writeFile : Text -> Text -> Result {} Text  - truncates then writes
    ("writeFile", "writeFile", concat!(
        "local writeFile = function(path) return function(content)\n",
        "  local f, err = io.open(path, \"w\")\n",
        "  if not f then return {_tag=\"Err\", reason=err} end\n",
        "  f:write(content)\n",
        "  f:close()\n",
        "  return {_tag=\"Ok\", value={}}\n",
        "end end\n\n",
    )),

    // appendFile : Text -> Text -> Result {} Text  - appends without truncating
    ("appendFile", "appendFile", concat!(
        "local appendFile = function(path) return function(content)\n",
        "  local f, err = io.open(path, \"a\")\n",
        "  if not f then return {_tag=\"Err\", reason=err} end\n",
        "  f:write(content)\n",
        "  f:close()\n",
        "  return {_tag=\"Ok\", value={}}\n",
        "end end\n\n",
    )),

    // ── Bool ───────────────────────────────────────────────────────────────
    // `not` is a Lua keyword so the function form must be renamed.
    ("not", "_not", "local _not = function(b) return not b end\n\n"),

    // ── Math ───────────────────────────────────────────────────────────────
    ("abs",   "abs",   "local abs   = function(n) return math.abs(n) end\n\n"),
    ("round", "round", "local round = function(n) return math.floor(n + 0.5) end\n\n"),
    ("floor", "floor", "local floor = function(n) return math.floor(n) end\n\n"),
    ("ceil",  "ceil",  "local ceil  = function(n) return math.ceil(n) end\n\n"),
    ("max",   "max",   "local max = function(a) return function(b) return math.max(a, b) end end\n\n"),
    ("min",   "min",   "local min = function(a) return function(b) return math.min(a, b) end end\n\n"),
    ("mod",   "mod",   "local mod = function(a) return function(b) return a % b end end\n\n"),
    ("pow",   "pow",   "local pow = function(a) return function(b) return a ^ b end end\n\n"),
    // toNum : Text -> Maybe Num
    ("toNum", "toNum", concat!(
        "local toNum = function(s)\n",
        "  local n = tonumber(s)\n",
        "  if n then return {_tag=\"Some\", value=n} else return {_tag=\"None\"} end\n",
        "end\n\n",
    )),
    // range : Num -> Num -> List Num  (inclusive on both ends)
    ("range", "range", concat!(
        "local range = function(from) return function(to)\n",
        "  local r = {}\n",
        "  for i = math.floor(from), math.floor(to) do r[#r+1] = i end\n",
        "  return r\n",
        "end end\n\n",
    )),

    // ── List ───────────────────────────────────────────────────────────────
    // All implemented with O(n) Lua loops so they are safe on large lists
    // regardless of LuaJIT's call-stack depth.
    ("map", "map", concat!(
        "local map = function(f) return function(xs)\n",
        "  local r = {}\n",
        "  for i = 1, #xs do r[i] = f(xs[i]) end\n",
        "  return r\n",
        "end end\n\n",
    )),
    ("filter", "filter", concat!(
        "local filter = function(f) return function(xs)\n",
        "  local r = {}\n",
        "  for _, v in ipairs(xs) do\n",
        "    if f(v) then r[#r+1] = v end\n",
        "  end\n",
        "  return r\n",
        "end end\n\n",
    )),
    // fold : b -> (b -> a -> b) -> List a -> b
    // The curried signature matches Lume's calling convention exactly.
    ("fold", "fold", concat!(
        "local fold = function(acc) return function(f) return function(xs)\n",
        "  local a = acc\n",
        "  for _, v in ipairs(xs) do a = f(a)(v) end\n",
        "  return a\n",
        "end end end\n\n",
    )),
    ("length",  "length",  "local length  = function(xs) return #xs end\n\n"),
    ("reverse", "reverse", concat!(
        "local reverse = function(xs)\n",
        "  local r = {}\n",
        "  for i = #xs, 1, -1 do r[#r+1] = xs[i] end\n",
        "  return r\n",
        "end\n\n",
    )),
    ("take", "take", concat!(
        "local take = function(n) return function(xs)\n",
        "  local r = {}\n",
        "  for i = 1, math.min(math.floor(n), #xs) do r[i] = xs[i] end\n",
        "  return r\n",
        "end end\n\n",
    )),
    ("drop", "drop", concat!(
        "local drop = function(n) return function(xs)\n",
        "  local r = {}\n",
        "  local start = math.floor(n) + 1\n",
        "  for i = start, #xs do r[#r+1] = xs[i] end\n",
        "  return r\n",
        "end end\n\n",
    )),
    ("any", "any", concat!(
        "local any = function(f) return function(xs)\n",
        "  for _, v in ipairs(xs) do\n",
        "    if f(v) then return true end\n",
        "  end\n",
        "  return false\n",
        "end end\n\n",
    )),
    ("all", "all", concat!(
        "local all = function(f) return function(xs)\n",
        "  for _, v in ipairs(xs) do\n",
        "    if not f(v) then return false end\n",
        "  end\n",
        "  return true\n",
        "end end\n\n",
    )),
    ("sum", "sum", concat!(
        "local sum = function(xs)\n",
        "  local s = 0\n",
        "  for _, v in ipairs(xs) do s = s + v end\n",
        "  return s\n",
        "end\n\n",
    )),
    ("average", "average", concat!(
        "local average = function(xs)\n",
        "  if #xs == 0 then return 0 end\n",
        "  local s = 0\n",
        "  for _, v in ipairs(xs) do s = s + v end\n",
        "  return s / #xs\n",
        "end\n\n",
    )),
    ("sort", "sort", concat!(
        "local sort = function(xs)\n",
        "  local r = {table.unpack(xs)}\n",
        "  table.sort(r)\n",
        "  return r\n",
        "end\n\n",
    )),
    ("sortBy", "sortBy", concat!(
        "local sortBy = function(f) return function(xs)\n",
        "  local r = {table.unpack(xs)}\n",
        "  table.sort(r, function(a, b) return f(a) < f(b) end)\n",
        "  return r\n",
        "end end\n\n",
    )),

    // ── Text ───────────────────────────────────────────────────────────────
    ("trim",       "trim",       "local trim = function(s) return (s:match(\"^%s*(.-)%s*$\")) end\n\n"),
    ("toUpper",    "toUpper",    "local toUpper = function(s) return s:upper() end\n\n"),
    ("toLower",    "toLower",    "local toLower = function(s) return s:lower() end\n\n"),
    ("split", "split", concat!(
        "local split = function(sep) return function(s)\n",
        "  local r = {}\n",
        "  if sep == \"\" then\n",
        "    for c in s:gmatch(\".\") do r[#r+1] = c end\n",
        "    return r\n",
        "  end\n",
        "  local i = 1\n",
        "  while true do\n",
        "    local j = s:find(sep, i, true)\n",
        "    if not j then r[#r+1] = s:sub(i); break end\n",
        "    r[#r+1] = s:sub(i, j - 1)\n",
        "    i = j + #sep\n",
        "  end\n",
        "  return r\n",
        "end end\n\n",
    )),
    ("join",       "join",       "local join = function(sep) return function(xs) return table.concat(xs, sep) end end\n\n"),
    ("contains",   "contains",   "local contains   = function(needle) return function(hay) return hay:find(needle, 1, true) ~= nil end end\n\n"),
    ("startsWith", "startsWith", "local startsWith = function(pre) return function(s) return s:sub(1, #pre) == pre end end\n\n"),
    ("endsWith",   "endsWith",   "local endsWith   = function(suf) return function(s) return suf == \"\" or s:sub(-#suf) == suf end end\n\n"),

    // ── Result / Maybe helpers ──────────────────────────────────────────────
    ("withDefault", "withDefault", concat!(
        "local withDefault = function(d) return function(m)\n",
        "  if m._tag == \"Some\" then return m.value else return d end\n",
        "end end\n\n",
    )),
    ("mapErr", "mapErr", concat!(
        "local mapErr = function(f) return function(r)\n",
        "  if r._tag == \"Ok\" then return r\n",
        "  else return {_tag=\"Err\", reason=f(r.reason)} end\n",
        "end end\n\n",
    )),
    // unwrap crashes on Err; uses _show so show must be emitted first.
    ("unwrap", "unwrap", concat!(
        "local unwrap = function(r)\n",
        "  if r._tag == \"Ok\" then return r.value\n",
        "  else error(\"unwrap: Err { reason: \" .. _show(r.reason) .. \" }\") end\n",
        "end\n\n",
    )),
];

/// Emit a Lua table key safely: bare identifier for normal names,
/// `["keyword"]` for Lua reserved words.
fn lua_field_key(name: &str) -> String {
    #[rustfmt::skip]
    const RESERVED: &[&str] = &[
        "and", "break", "do", "else", "elseif", "end", "false", "for",
        "function", "goto", "if", "in", "local", "nil", "not", "or",
        "repeat", "return", "then", "true", "until", "while",
    ];
    if RESERVED.contains(&name) {
        format!("[\"{}\"]", name)
    } else {
        name.to_string()
    }
}

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
        needs_concat: false,
        needed_stdlib: HashSet::new(),
        module_vars,
    };

    let last = bundle.len().saturating_sub(1);
    for (i, m) in bundle.iter().enumerate() {
        e.emit_module(&m.program, &m.canonical, &m.var, i == last);
    }

    // Build the preamble. Internal code-gen helpers come first, then stdlib
    // functions in STDLIB order (which preserves dependency relationships).
    let mut preamble = String::new();

    if e.needs_result_bind {
        preamble.push_str(
            "local function _resultBind(r, f)\n\
             \x20 if r._tag == \"Ok\" then return f(r.value) else return r end\n\
             end\n\n",
        );
    }
    if e.needs_extend {
        preamble.push_str(
            "local function _extend(t, u)\n\
             \x20 local r = {}\n\
             \x20 for k, v in pairs(t) do r[k] = v end\n\
             \x20 for k, v in pairs(u) do r[k] = v end\n\
             \x20 return r\n\
             end\n\n",
        );
    }
    if e.needs_slice {
        preamble.push_str(
            "local function _slice(t, i)\n\
             \x20 local r = {}\n\
             \x20 for j = i, #t do r[#r + 1] = t[j] end\n\
             \x20 return r\n\
             end\n\n",
        );
    }
    if e.needs_concat {
        preamble.push_str(
            "local function _concat(a, b)\n\
             \x20 if type(a) == \"string\" then return a .. b end\n\
             \x20 local r = {}\n\
             \x20 for _, v in ipairs(a) do r[#r+1] = v end\n\
             \x20 for _, v in ipairs(b) do r[#r+1] = v end\n\
             \x20 return r\n\
             end\n\n",
        );
    }
    if e.needs_omit {
        preamble.push_str(
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

    // `unwrap` calls `_show`, so ensure show is emitted whenever unwrap is used.
    if e.needed_stdlib.contains("unwrap") {
        e.needed_stdlib.insert("show".to_string());
    }

    for (lume_name, _lua_name, impl_str) in STDLIB {
        if e.needed_stdlib.contains(*lume_name) {
            preamble.push_str(impl_str);
        }
    }

    if !preamble.is_empty() {
        e.out.insert_str(0, &preamble);
    }
    e.out
}

struct Emitter {
    out: String,
    tmp: usize,
    // Internal code-generation helpers (triggered by language constructs).
    needs_extend: bool,
    needs_slice: bool,
    needs_omit: bool,
    needs_result_bind: bool,
    needs_concat: bool,
    // Stdlib functions referenced by name in the program.
    needed_stdlib: HashSet<String>,
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
                TopItem::TypeDef(_) | TopItem::TraitDef(_) | TopItem::ImplDef(_) => {}
                TopItem::Binding(b) => {
                    self.emit_binding(b);
                    self.out.push('\n');
                }
                TopItem::BindingGroup(bs) => {
                    self.emit_binding_group(bs);
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
        // Stdlib paths use the synthetic key produced by `stdlib_path`.
        let mod_var = if crate::loader::stdlib_source(&u.path).is_some() {
            self.module_vars
                .get(&crate::loader::stdlib_path(&u.path))
                .cloned()
        } else {
            crate::loader::resolve_path(&u.path, base)
                .ok()
                .and_then(|p| self.module_vars.get(&p).cloned())
        };

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
                // Not in bundle - fall back to require().
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
                        self.out
                            .push_str(&format!("local {} = require(\"{}\")\n", tmp, path));
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

    /// Emit a mutually recursive binding group.
    ///
    /// All names are declared with a single `local` statement first, so every
    /// body in the group can close over every other name.
    fn emit_binding_group(&mut self, bs: &[Binding]) {
        // Collect names of simple Ident patterns.
        let names: Vec<String> = bs
            .iter()
            .filter_map(|b| {
                if let Pattern::Ident(name, _, _) = &b.pattern {
                    Some(lua_ident(name).into_owned())
                } else {
                    None
                }
            })
            .collect();

        if !names.is_empty() {
            self.out.push_str(&format!("local {}\n", names.join(", ")));
        }

        // Now emit each assignment (without the `local` prefix).
        for b in bs {
            match &b.pattern {
                Pattern::Ident(name, _, _) => {
                    let n = lua_ident(name).into_owned();
                    self.out.push_str(&format!("{} = ", n));
                    self.emit_expr(&b.value);
                    self.out.push('\n');
                }
                _ => {
                    // Non-ident patterns fall back to the normal path.
                    self.emit_pat_binding(&b.pattern, &b.value);
                    self.out.push('\n');
                }
            }
        }
    }

    /// Emit one or more `local` statements for `pat = expr`.
    fn emit_pat_binding(&mut self, pat: &Pattern, expr: &Expr) {
        match pat {
            Pattern::Ident(name, _, _) => {
                let n = lua_ident(name).into_owned();
                self.out.push_str(&format!("local {}\n", n));
                self.out.push_str(&format!("{} = ", n));
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
                // Check if this is a stdlib function. If so, record it (so the
                // preamble implementation is emitted) and use the Lua-side name.
                if let Some((_, lua_name, _)) = STDLIB.iter().find(|(n, _, _)| *n == name.as_str())
                {
                    self.needed_stdlib.insert(name.clone());
                    self.out.push_str(lua_name);
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
                        self.out.push_str(&format!("{} = ", lua_field_key(&f.name)));
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
                        self.out.push_str(&format!("{} = ", lua_field_key(&f.name)));
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
                // Use bracket syntax for Lua reserved words.
                #[rustfmt::skip]
                const RESERVED: &[&str] = &[
                    "and", "break", "do", "else", "elseif", "end", "false", "for",
                    "function", "goto", "if", "in", "local", "nil", "not", "or",
                    "repeat", "return", "then", "true", "until", "while",
                ];
                if RESERVED.contains(&field.as_str()) {
                    self.out.push_str(&format!("[\"{}\"]", field));
                } else {
                    self.out.push('.');
                    self.out.push_str(field);
                }
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
            ExprKind::TraitCall { trait_name, method_name } => {
                // A TraitCall with an ambiguous (polymorphic) type survives desugaring.
                // Emit a function that errors when called; the fix is a type annotation.
                self.out.push_str(&format!(
                    "function() error(\"ambiguous trait call {}.{}: add a type annotation\") end",
                    trait_name, method_name
                ));
            }
            ExprKind::LetIn {
                pattern,
                value,
                body,
            } => {
                // Emit as IIFE: (function(param) return body end)(value)
                self.out.push('(');
                self.emit_lambda(pattern, body);
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

    /// Emit an expression that is in return position.
    ///
    /// When the expression is an `if` or `let-in`, emit the Lua statement form
    /// directly instead of wrapping it in an immediately-invoked closure.  This
    /// keeps tail calls visible to LuaJIT so that recursive functions over large
    /// lists do not blow the call stack.
    ///
    /// For every other expression kind, this is identical to `return <emit_expr>`.
    fn emit_tail_expr(&mut self, expr: &Expr, indent: &str) {
        match &expr.kind {
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.out.push_str("if ");
                self.emit_expr(cond);
                self.out.push_str(" then\n");
                self.out.push_str(&format!("{}  ", indent));
                self.emit_tail_expr(then_branch, &format!("{}  ", indent));
                self.out.push_str(&format!("\n{}else\n", indent));
                self.out.push_str(&format!("{}  ", indent));
                self.emit_tail_expr(else_branch, &format!("{}  ", indent));
                self.out.push_str(&format!("\n{}end", indent));
            }
            ExprKind::LetIn {
                pattern,
                value,
                body,
            } => {
                // Emit as local bindings + tail body - no IIFE, so LuaJIT can
                // see the tail call in `body` as a proper tail call.
                match pattern {
                    Pattern::Ident(name, _, _) => {
                        self.out.push_str(&format!("local {} = ", lua_ident(name)));
                        self.emit_expr(value);
                        self.out.push_str(&format!("\n{}", indent));
                        self.emit_tail_expr(body, indent);
                    }
                    Pattern::Wildcard => {
                        self.emit_expr(value);
                        self.out.push_str(&format!("\n{}", indent));
                        self.emit_tail_expr(body, indent);
                    }
                    Pattern::Record(rp) => {
                        // Collect bindings first (immutable borrow), then emit.
                        let mut all_binds: Vec<(String, String)> = Vec::new();
                        for f in &rp.fields {
                            if let Some(p) = &f.pattern {
                                let b = self.collect_binds_pure(&format!("_lv.{}", f.name), p);
                                all_binds.extend(b);
                            } else {
                                all_binds.push((
                                    lua_ident(&f.name).to_string(),
                                    format!("_lv.{}", f.name),
                                ));
                            }
                        }
                        self.out.push_str("local _lv = ");
                        self.emit_expr(value);
                        self.out.push('\n');
                        for (lhs, rhs) in &all_binds {
                            self.out
                                .push_str(&format!("{}local {} = {}\n", indent, lhs, rhs));
                        }
                        self.out.push_str(indent);
                        self.emit_tail_expr(body, indent);
                    }
                    _ => {
                        // Refutable pattern: fall back to IIFE (rare in let-in).
                        self.out.push_str("return (");
                        self.emit_lambda(pattern, body);
                        self.out.push_str(")(");
                        self.emit_expr(value);
                        self.out.push(')');
                    }
                }
            }
            _ => {
                self.out.push_str("return ");
                self.emit_expr(expr);
            }
        }
    }

    fn emit_lambda(&mut self, param: &Pattern, body: &Expr) {
        match param {
            Pattern::Ident(name, _, _) => {
                self.out
                    .push_str(&format!("function({})\n  ", lua_ident(name)));
                self.emit_tail_expr(body, "  ");
                self.out.push_str("\nend");
            }
            Pattern::Wildcard => {
                self.out.push_str("function(_)\n  ");
                self.emit_tail_expr(body, "  ");
                self.out.push_str("\nend");
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
                self.out.push_str("  ");
                self.emit_tail_expr(body, "  ");
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
                    self.out.push_str("  ");
                    self.emit_tail_expr(body, "  ");
                    self.out.push_str("\nend");
                } else {
                    self.out.push_str(&format!("  if {} then\n    ", cond));
                    self.emit_tail_expr(body, "    ");
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
                self.needs_concat = true;
                self.out.push_str("_concat(");
                self.emit_expr(left);
                self.out.push_str(", ");
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
                self.out.push_str("    ");
                self.emit_tail_expr(&arm.body, "    ");
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
                self.out.push_str(" then\n      ");
                self.emit_tail_expr(&arm.body, "      ");
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
