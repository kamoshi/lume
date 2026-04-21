use crate::builtin::{BUILTINS, MAP_BUILTINS};
use crate::codegen::IrModule;
use crate::ir;
use crate::ir::{BinOp, UnOp};
use crate::types::infer::VariantEnv;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// All user-visible stdlib functions.
///
/// Order is significant: functions that appear earlier are emitted first, so
/// dependencies (e.g. `_show` needed by `unwrap`) are satisfied.
///
/// Each entry is `(lume_name, lua_name, lua_implementation)`.
static STDLIB: &[(&str, &str, &str)] = &[
    // ── Internal _show (not user-facing; used by unwrap) ──────────────────
    ("_show_internal", "_show", concat!(
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

    // ── Typed show primitives ─────────────────────────────────────────────
    ("showNum", "_showNum", concat!(
        "local function _showNum(x)\n",
        "  if x == math.floor(x) then return tostring(math.floor(x)) else return tostring(x) end\n",
        "end\n\n",
    )),
    ("showBool", "_showBool", "local function _showBool(x) return x and \"true\" or \"false\" end\n\n"),
    ("showText", "_showText", "local function _showText(x) return x end\n\n"),
    ("showRecord", "_showRecord", concat!(
        "local function _showRecord(x)\n",
        "  if type(x) ~= \"table\" then return tostring(x) end\n",
        "  if x._tag ~= nil then\n",
        "    local parts = {}\n",
        "    for k, v in pairs(x) do\n",
        "      if k ~= \"_tag\" then parts[#parts+1] = k .. \": \" .. _show(v) end\n",
        "    end\n",
        "    if #parts == 0 then return x._tag end\n",
        "    return x._tag .. \" { \" .. table.concat(parts, \", \") .. \" }\"\n",
        "  end\n",
        "  local parts = {}\n",
        "  for k, v in pairs(x) do parts[#parts+1] = k .. \": \" .. _show(v) end\n",
        "  if #parts == 0 then return \"{}\" end\n",
        "  return \"{ \" .. table.concat(parts, \", \") .. \" }\"\n",
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
    ("list_map", "list_map", concat!(
        "local list_map = function(f) return function(xs)\n",
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
        "  local r = {}\n",
        "  for i = 1, #xs do r[i] = xs[i] end\n",
        "  table.sort(r)\n",
        "  return r\n",
        "end\n\n",
    )),
    ("sortBy", "sortBy", concat!(
        "local sortBy = function(f) return function(xs)\n",
        "  local r = {}\n",
        "  for i = 1, #xs do r[i] = xs[i] end\n",
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

#[rustfmt::skip]
const LUA_RESERVED: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for",
    "function", "goto", "if", "in", "local", "nil", "not", "or",
    "repeat", "return", "then", "true", "until", "while",
];

/// Emit a Lua table key safely: bare identifier for normal names,
/// `["keyword"]` for Lua reserved words.
fn lua_field_key(name: &str) -> String {
    if LUA_RESERVED.contains(&name) || !is_lua_ident(name) {
        // Escape backslashes inside the Lua string key.
        let escaped = name.replace('\\', "\\\\");
        format!("[\"{}\"]", escaped)
    } else {
        name.to_string()
    }
}

/// Check if a name is a valid Lua identifier.
fn is_lua_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Escape Lua reserved words and mangle operator-named identifiers.
fn lua_ident(name: &str) -> std::borrow::Cow<'_, str> {
    if LUA_RESERVED.contains(&name) {
        std::borrow::Cow::Owned(format!("_{}", name))
    } else if !is_lua_ident(name) {
        std::borrow::Cow::Owned(mangle_op(name))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// Mangle an operator name into a valid Lua identifier.
fn mangle_op(name: &str) -> String {
    let mut buf = String::from("__op_");
    for c in name.chars() {
        match c {
            '+' => buf.push_str("plus"),
            '-' => buf.push_str("minus"),
            '*' => buf.push_str("star"),
            '/' => buf.push_str("slash"),
            '=' => buf.push_str("eq"),
            '<' => buf.push_str("lt"),
            '>' => buf.push_str("gt"),
            '!' => buf.push_str("bang"),
            '|' => buf.push_str("pipe"),
            '&' => buf.push_str("amp"),
            '?' => buf.push_str("qmark"),
            '^' => buf.push_str("caret"),
            '~' => buf.push_str("tilde"),
            '$' => buf.push_str("dollar"),
            '#' => buf.push_str("hash"),
            '@' => buf.push_str("at"),
            '%' => buf.push_str("pct"),
            '\\' => buf.push_str("bslash"),
            '.' => buf.push_str("dot"),
            ':' => buf.push_str("colon"),
            _ => buf.push(c),
        }
    }
    buf
}

/// All stdlib + builtin implementations as globals, for REPL preloading.
/// Each STDLIB entry has its leading `local ` stripped so the definitions
/// land in the global table. BUILTIN entries are emitted as `name = body`.
pub fn full_prelude() -> String {
    let mut out = String::new();

    // Internal helpers — written directly without `local`.
    out.push_str(
        "function _resultBind(r, f)\n\
         \x20 if r._tag == \"Ok\" then return f(r._0) else return r end\n\
         end\n\n",
    );
    out.push_str(
        "function _extend(t, u)\n\
         \x20 local r = {}\n\
         \x20 for k, v in pairs(t) do r[k] = v end\n\
         \x20 for k, v in pairs(u) do r[k] = v end\n\
         \x20 return r\n\
         end\n\n",
    );
    out.push_str(
        "function _slice(t, i)\n\
         \x20 local r = {}\n\
         \x20 for j = i, #t do r[#r + 1] = t[j] end\n\
         \x20 return r\n\
         end\n\n",
    );
    out.push_str(
        "function _concat(a, b)\n\
         \x20 if type(a) == \"string\" then return a .. b end\n\
         \x20 local r = {}\n\
         \x20 for _, v in ipairs(a) do r[#r+1] = v end\n\
         \x20 for _, v in ipairs(b) do r[#r+1] = v end\n\
         \x20 return r\n\
         end\n\n",
    );
    out.push_str(
        "function _omit(t, keys)\n\
         \x20 local r = {}\n\
         \x20 local s = {}\n\
         \x20 for _, k in ipairs(keys) do s[k] = true end\n\
         \x20 for k, v in pairs(t) do\n\
         \x20\x20\x20 if not s[k] then r[k] = v end\n\
         \x20 end\n\
         \x20 return r\n\
         end\n\n",
    );

    // STDLIB: strip leading `local ` to make definitions global.
    for (_, _, impl_str) in STDLIB {
        out.push_str(impl_str.strip_prefix("local ").unwrap_or(impl_str));
    }

    // BUILTINS: bare assignment.
    for b in BUILTINS.iter().chain(MAP_BUILTINS.iter()) {
        out.push_str(&format!("{} = {}\n\n", b.lua_name(), b.lua));
    }

    out
}

/// Emit new REPL bindings as bare assignments (no `local`, no preamble, no
/// `return`). `skip` is the number of IR items already loaded into the Lua
/// state from previous evals and should not be re-emitted.
/// `module_vars` maps canonical dep paths to the global variable names under
/// which their exports are stored (populated by [`emit_dep_modules`]).
pub fn emit_repl(
    module: &IrModule,
    variant_env: VariantEnv,
    skip: usize,
    module_vars: HashMap<PathBuf, String>,
) -> String {
    let mut e = Emitter {
        out: String::new(),
        tmp: 0,
        repl: true,
        needs_extend: false,
        needs_slice: false,
        needs_omit: false,
        needs_result_bind: false,
        needs_concat: false,
        needed_stdlib: HashSet::new(),
        module_vars,
        variant_env,
    };

    // Emit import bindings (persistent bare assignments, no `local`).
    for u in &module.module.imports {
        e.emit_import(u, &module.canonical);
    }
    if !module.module.imports.is_empty() {
        e.out.push('\n');
    }

    for item in module.module.items.iter().skip(skip) {
        match item {
            ir::Decl::Let(pat, expr) => {
                e.emit_pat_binding(pat, expr);
                e.out.push('\n');
            }
            ir::Decl::LetRec(bindings) => {
                e.emit_binding_group(bindings);
                e.out.push('\n');
            }
        }
    }

    e.out
}

/// Emit dependency modules as global IIFE assignments for the REPL.
/// Each module becomes `_mod_foo = (function() … return exports end)()`.
/// `module_vars` must contain entries for *all* transitive deps so that
/// inter-module imports resolve correctly.
pub fn emit_dep_modules(
    modules: &[&IrModule],
    module_vars: HashMap<PathBuf, String>,
    variant_env: VariantEnv,
) -> String {
    let mut e = Emitter {
        out: String::new(),
        tmp: 0,
        repl: false,
        needs_extend: false,
        needs_slice: false,
        needs_omit: false,
        needs_result_bind: false,
        needs_concat: false,
        needed_stdlib: HashSet::new(),
        module_vars,
        variant_env,
    };
    for m in modules {
        e.emit_module_global(&m.module, &m.canonical, &m.var);
    }
    e.out
}

pub fn emit(bundle: &[IrModule], variant_env: VariantEnv) -> String {
    let module_vars: HashMap<PathBuf, String> = bundle
        .iter()
        .map(|m| (m.canonical.clone(), m.var.clone()))
        .collect();

    let mut e = Emitter {
        out: String::new(),
        tmp: 0,
        repl: false,
        needs_extend: false,
        needs_slice: false,
        needs_omit: false,
        needs_result_bind: false,
        needs_concat: false,
        needed_stdlib: HashSet::new(),
        module_vars,
        variant_env,
    };

    let last = bundle.len().saturating_sub(1);
    for (i, m) in bundle.iter().enumerate() {
        e.emit_module(&m.module, &m.canonical, &m.var, i == last);
    }

    // Build the preamble. Internal code-gen helpers come first, then stdlib
    // functions in STDLIB order (which preserves dependency relationships).
    let mut preamble = String::new();

    if e.needs_result_bind {
        preamble.push_str(
            "local function _resultBind(r, f)\n\
             \x20 if r._tag == \"Ok\" then return f(r._0) else return r end\n\
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

    // `unwrap` calls `_show`, so ensure internal show is emitted whenever unwrap is used.
    if e.needed_stdlib.contains("unwrap") {
        e.needed_stdlib.insert("_show_internal".to_string());
    }
    // `showRecord` also depends on `_show` for nested values.
    if e.needed_stdlib.contains("showRecord") {
        e.needed_stdlib.insert("_show_internal".to_string());
    }

    for (lume_name, _lua_name, impl_str) in STDLIB {
        if e.needed_stdlib.contains(*lume_name) {
            preamble.push_str(impl_str);
        }
    }
    for b in BUILTINS.iter().chain(MAP_BUILTINS.iter()) {
        if e.needed_stdlib.contains(b.name) {
            preamble.push_str(&format!("local {} = {}\n\n", b.lua_name(), b.lua));
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
    /// When true, top-level `Pat::Var` bindings are emitted as bare assignments
    /// (`x = expr`) instead of `local x\nx = expr`. Used by the REPL so
    /// bindings land in the persistent env rather than as chunk-local variables.
    repl: bool,
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
    /// Variant metadata for wrapper variant codegen.
    variant_env: VariantEnv,
}

impl Emitter {
    fn fresh(&mut self) -> String {
        let n = self.tmp;
        self.tmp += 1;
        format!("_t{}", n)
    }

    fn emit_module(&mut self, module: &ir::Module, canonical: &Path, var: &str, is_entry: bool) {
        if !is_entry {
            self.out.push_str(&format!("local {} = (function()\n", var));
        }

        for u in &module.imports {
            self.emit_import(u, canonical);
        }
        if !module.imports.is_empty() {
            self.out.push('\n');
        }

        for item in &module.items {
            match item {
                ir::Decl::Let(pat, expr) => {
                    self.emit_pat_binding(pat, expr);
                    self.out.push('\n');
                }
                ir::Decl::LetRec(bindings) => {
                    self.emit_binding_group(bindings);
                    self.out.push('\n');
                }
            }
        }

        self.out.push_str("\nreturn ");
        self.emit_expr(&module.exports);
        self.out.push('\n');

        if !is_entry {
            self.out.push_str("end)()\n\n");
        }
    }

    fn emit_import(&mut self, u: &ir::Import, base: &Path) {
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

        // In REPL mode emit bare assignments so bindings persist across chunks.
        let local = if self.repl { "" } else { "local " };

        match mod_var {
            Some(mv) => match &u.binding {
                ir::ImportBinding::Name(name) => {
                    self.out
                        .push_str(&format!("{}{} = {}\n", local, lua_ident(name), mv));
                }
                ir::ImportBinding::Destructure(names) => {
                    for name in names {
                        let key = lua_field_key(name);
                        let access = if key.starts_with('[') {
                            format!("{}{}", mv, key)
                        } else {
                            format!("{}.{}", mv, key)
                        };
                        self.out.push_str(&format!(
                            "{}{} = {}\n",
                            local,
                            lua_ident(name),
                            access
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
                    ir::ImportBinding::Name(name) => {
                        self.out.push_str(&format!(
                            "{}{} = require(\"{}\")\n",
                            local,
                            lua_ident(name),
                            path
                        ));
                    }
                    ir::ImportBinding::Destructure(names) => {
                        let tmp = self.fresh();
                        // Always use `local` for the temp var even in REPL mode.
                        self.out
                            .push_str(&format!("local {} = require(\"{}\")\n", tmp, path));
                        for name in names {
                            let key = lua_field_key(name);
                            let access = if key.starts_with('[') {
                                format!("{}{}", tmp, key)
                            } else {
                                format!("{}.{}", tmp, key)
                            };
                            self.out.push_str(&format!(
                                "{}{} = {}\n",
                                local,
                                lua_ident(name),
                                access
                            ));
                        }
                    }
                }
            }
        }
    }

    /// Like `emit_module` for non-entry modules but assigns to a global
    /// variable instead of `local`, so the binding outlives the chunk.
    /// Used when loading dependency modules into the REPL Lua state.
    fn emit_module_global(&mut self, module: &ir::Module, canonical: &Path, var: &str) {
        self.out.push_str(&format!("{} = (function()\n", var));

        for u in &module.imports {
            self.emit_import(u, canonical);
        }
        if !module.imports.is_empty() {
            self.out.push('\n');
        }

        for item in &module.items {
            match item {
                ir::Decl::Let(pat, expr) => {
                    self.emit_pat_binding(pat, expr);
                    self.out.push('\n');
                }
                ir::Decl::LetRec(bindings) => {
                    self.emit_binding_group(bindings);
                    self.out.push('\n');
                }
            }
        }

        self.out.push_str("\nreturn ");
        self.emit_expr(&module.exports);
        self.out.push('\n');
        self.out.push_str("end)()\n\n");
    }

    /// Emit a mutually recursive binding group.
    ///
    /// All names are declared with a single `local` statement first, so every
    /// body in the group can close over every other name.
    fn emit_binding_group(&mut self, bs: &[(ir::Pat, ir::Expr)]) {
        // Collect names of simple Var patterns.
        let names: Vec<String> = bs
            .iter()
            .filter_map(|(pat, _)| {
                if let ir::Pat::Var(name) = pat {
                    Some(lua_ident(name).into_owned())
                } else {
                    None
                }
            })
            .collect();

        if !names.is_empty() && !self.repl {
            self.out.push_str(&format!("local {}\n", names.join(", ")));
        }

        // Now emit each assignment (without the `local` prefix).
        for (pat, expr) in bs {
            match pat {
                ir::Pat::Var(name) => {
                    let n = lua_ident(name).into_owned();
                    self.out.push_str(&format!("{} = ", n));
                    self.emit_expr(expr);
                    self.out.push('\n');
                }
                _ => {
                    // Non-ident patterns fall back to the normal path.
                    self.emit_pat_binding(pat, expr);
                    self.out.push('\n');
                }
            }
        }
    }

    /// Emit one or more `local` statements for `pat = expr`.
    fn emit_pat_binding(&mut self, pat: &ir::Pat, expr: &ir::Expr) {
        match pat {
            ir::Pat::Var(name) => {
                let n = lua_ident(name).into_owned();
                if !self.repl {
                    self.out.push_str(&format!("local {}\n", n));
                }
                self.out.push_str(&format!("{} = ", n));
                self.emit_expr(expr);
            }
            ir::Pat::Wild => {
                self.out.push_str("local _ = ");
                self.emit_expr(expr);
            }
            ir::Pat::Record { fields, rest } => {
                let tmp = self.fresh();
                self.out.push_str(&format!("local {} = ", tmp));
                self.emit_expr(expr);
                self.out.push('\n');
                for (name, pat_opt) in fields {
                    let src = format!("{}.{}", tmp, name);
                    if let Some(p) = pat_opt {
                        let binds = self.collect_binds_pure(&src, p);
                        for (lhs, rhs) in binds {
                            self.out.push_str(&format!("local {} = {}\n", lhs, rhs));
                        }
                    } else {
                        self.out
                            .push_str(&format!("local {} = {}\n", lua_ident(name), src));
                    }
                }
                if let Some(Some(rest_name)) = rest {
                    self.needs_omit = true;
                    let excluded: Vec<_> = fields
                        .iter()
                        .map(|(name, _)| format!("\"{}\"", name))
                        .collect();
                    self.out.push_str(&format!(
                        "local {} = _omit({}, {{{}}})\n",
                        rest_name,
                        tmp,
                        excluded.join(", ")
                    ));
                }
            }
            ir::Pat::List { elems, rest } => {
                let tmp = self.fresh();
                self.out.push_str(&format!("local {} = ", tmp));
                self.emit_expr(expr);
                self.out.push('\n');
                for (i, elem) in elems.iter().enumerate() {
                    let src = format!("{}[{}]", tmp, i + 1);
                    let binds = self.collect_binds_pure(&src, elem);
                    for (lhs, rhs) in binds {
                        self.out.push_str(&format!("local {} = {}\n", lhs, rhs));
                    }
                }
                if let Some(Some(rest_name)) = rest {
                    self.needs_slice = true;
                    self.out.push_str(&format!(
                        "local {} = _slice({}, {})\n",
                        rest_name,
                        tmp,
                        elems.len() + 1
                    ));
                }
            }
            _ => {
                self.out.push_str("local _ = ");
                self.emit_expr(expr);
            }
        }
    }

    fn emit_expr(&mut self, expr: &ir::Expr) {
        match expr {
            ir::Expr::Num(n) => self.emit_number(*n),
            ir::Expr::Str(s) => self.emit_string(s),
            ir::Expr::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            ir::Expr::List { bases, elems } => {
                if bases.is_empty() {
                    // Plain list literal: { elem, elem, ... }
                    self.out.push('{');
                    for (i, item) in elems.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.emit_expr(item);
                    }
                    self.out.push('}');
                } else {
                    // Chain _concat calls for spread bases + trailing elems
                    self.needs_concat = true;
                    let trailing = if elems.is_empty() {
                        None
                    } else {
                        Some(elems)
                    };
                    let total = bases.len() + if trailing.is_some() { 1 } else { 0 };
                    // Open _concat( wrappers
                    for _ in 1..total {
                        self.out.push_str("_concat(");
                    }
                    for (i, base) in bases.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.emit_expr(base);
                        if i > 0 {
                            self.out.push(')');
                        }
                    }
                    if let Some(elems) = trailing {
                        self.out.push_str(", {");
                        for (i, e) in elems.iter().enumerate() {
                            if i > 0 {
                                self.out.push_str(", ");
                            }
                            self.emit_expr(e);
                        }
                        self.out.push_str("})");
                    }
                }
            }
            ir::Expr::Var(name) => {
                // Check if this is a stdlib function. If so, record it (so the
                // preamble implementation is emitted) and use the Lua-side name.
                if let Some((_, lua_name, _)) = STDLIB.iter().find(|(n, _, _)| *n == name.as_str())
                {
                    self.needed_stdlib.insert(name.clone());
                    self.out.push_str(lua_name);
                } else if let Some(b) = BUILTINS.iter().chain(MAP_BUILTINS.iter()).find(|b| b.name == name.as_str()) {
                    self.needed_stdlib.insert(name.clone());
                    self.out.push_str(&b.lua_name());
                } else {
                    self.out.push_str(&lua_ident(name));
                }
            }
            ir::Expr::Record { bases, fields } => {
                if bases.is_empty() {
                    // Plain record literal: { field = val, ... }
                    self.out.push('{');
                    for (i, (name, val)) in fields.iter().enumerate() {
                        if i > 0 {
                            self.out.push_str(", ");
                        }
                        self.out.push_str(&format!("{} = ", lua_field_key(name)));
                        self.emit_expr(val);
                    }
                    self.out.push('}');
                } else {
                    // Record with bases: chain _extend calls left-to-right.
                    // _extend(_extend(b1, b2), { fields })
                    self.needs_extend = true;
                    // Build the nested _extend calls for all bases.
                    let extend_depth = bases.len() - 1 + if fields.is_empty() { 0 } else { 1 };
                    for _ in 0..extend_depth {
                        self.out.push_str("_extend(");
                    }
                    let mut first = true;
                    for base in bases {
                        if !first {
                            self.out.push_str(", ");
                            self.emit_expr(base);
                            self.out.push(')');
                        } else {
                            self.emit_expr(base);
                            first = false;
                        }
                    }
                    if !fields.is_empty() {
                        self.out.push_str(", {");
                        for (i, (name, val)) in fields.iter().enumerate() {
                            if i > 0 {
                                self.out.push_str(", ");
                            }
                            self.out.push_str(&format!("{} = ", lua_field_key(name)));
                            self.emit_expr(val);
                        }
                        self.out.push_str("})");
                    }
                }
            }
            ir::Expr::Field(record, field) => {
                self.emit_access_target(record);
                if LUA_RESERVED.contains(&field.as_str()) || !is_lua_ident(field) {
                    self.out.push_str(&format!("[\"{}\"]", field));
                } else {
                    self.out.push('.');
                    self.out.push_str(field);
                }
            }
            ir::Expr::Tag(name, payload) => match payload {
                None => {
                    self.out.push_str(&format!("{{_tag = \"{}\"}}", name));
                }
                Some(payload_expr) => {
                    self.out.push_str(&format!("{{_tag = \"{}\", _0 = ", name));
                    self.emit_expr(payload_expr);
                    self.out.push('}');
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
                UnOp::Neg => {
                    self.out.push('-');
                    self.emit_call_target(operand);
                }
                UnOp::Not => {
                    self.out.push_str("not ");
                    self.emit_call_target(operand);
                }
            },
            ir::Expr::If(cond, then_branch, else_branch) => {
                self.out.push_str("(function() if ");
                self.emit_expr(cond);
                self.out.push_str(" then return ");
                self.emit_expr(then_branch);
                self.out.push_str(" else return ");
                self.emit_expr(else_branch);
                self.out.push_str(" end end)()");
            }
            ir::Expr::MatchFn(arms) => self.emit_match_fn(arms),
            ir::Expr::Match(scrutinee, arms) => self.emit_match_expr(scrutinee, arms),
            ir::Expr::Let(pattern, value, body) => {
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

    /// Emit an expression that is in return position.
    ///
    /// When the expression is an `if` or `let-in`, emit the Lua statement form
    /// directly instead of wrapping it in an immediately-invoked closure.  This
    /// keeps tail calls visible to LuaJIT so that recursive functions over large
    /// lists do not blow the call stack.
    ///
    /// For every other expression kind, this is identical to `return <emit_expr>`.
    fn emit_tail_expr(&mut self, expr: &ir::Expr, indent: &str) {
        match expr {
            ir::Expr::If(cond, then_branch, else_branch) => {
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
            ir::Expr::Let(pattern, value, body) => {
                // Emit as local bindings + tail body - no IIFE, so LuaJIT can
                // see the tail call in `body` as a proper tail call.
                match pattern {
                    ir::Pat::Var(name) => {
                        self.out.push_str(&format!("local {} = ", lua_ident(name)));
                        self.emit_expr(value);
                        self.out.push_str(&format!("\n{}", indent));
                        self.emit_tail_expr(body, indent);
                    }
                    ir::Pat::Wild => {
                        self.emit_expr(value);
                        self.out.push_str(&format!("\n{}", indent));
                        self.emit_tail_expr(body, indent);
                    }
                    ir::Pat::Record { fields, .. } => {
                        // Collect bindings first (immutable borrow), then emit.
                        let mut all_binds: Vec<(String, String)> = Vec::new();
                        for (name, pat_opt) in fields {
                            if let Some(p) = pat_opt {
                                let b = self.collect_binds_pure(&format!("_lv.{}", name), p);
                                all_binds.extend(b);
                            } else {
                                all_binds.push((
                                    lua_ident(name).to_string(),
                                    format!("_lv.{}", name),
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
            ir::Expr::Match(scrutinee, arms) => {
                // Emit directly (no IIFE) so the Lua tail-call in each arm is
                // visible to LuaJIT, enabling O(1) stack for recursive fns.
                let tmp = self.fresh();
                self.out.push_str(&format!("local {} = ", tmp));
                self.emit_expr(scrutinee);
                self.out.push_str(&format!("\n{}", indent));
                self.emit_match_arms(&tmp, arms, indent);
            }
            _ => {
                self.out.push_str("return ");
                self.emit_expr(expr);
            }
        }
    }

    fn emit_lambda(&mut self, param: &ir::Pat, body: &ir::Expr) {
        match param {
            ir::Pat::Var(name) => {
                self.out
                    .push_str(&format!("function({})\n  ", lua_ident(name)));
                self.emit_tail_expr(body, "  ");
                self.out.push_str("\nend");
            }
            ir::Pat::Wild => {
                self.out.push_str("function(_)\n  ");
                self.emit_tail_expr(body, "  ");
                self.out.push_str("\nend");
            }
            ir::Pat::Record { fields, rest } => {
                self.out.push_str("function(_arg)\n");
                for (name, pat_opt) in fields {
                    if let Some(p) = pat_opt {
                        let binds = self.collect_binds_pure(&format!("_arg.{}", name), p);
                        for (lhs, rhs) in binds {
                            self.out.push_str(&format!("  local {} = {}\n", lhs, rhs));
                        }
                    } else {
                        self.out.push_str(&format!(
                            "  local {} = _arg.{}\n",
                            lua_ident(name),
                            name
                        ));
                    }
                }
                if let Some(Some(rest_name)) = rest {
                    self.needs_omit = true;
                    let excluded: Vec<_> = fields
                        .iter()
                        .map(|(name, _)| format!("\"{}\"", name))
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

    fn emit_binary(&mut self, op: &BinOp, left: &ir::Expr, right: &ir::Expr) {
        match op {
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

    fn emit_match_fn(&mut self, arms: &[ir::Branch]) {
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

    fn emit_match_expr(&mut self, scrutinee: &ir::Expr, arms: &[ir::Branch]) {
        let tmp = self.fresh();
        self.out.push_str("(function()\n");
        self.out.push_str(&format!("local {} = ", tmp));
        self.emit_expr(scrutinee);
        self.out.push('\n');
        self.emit_match_arms(&tmp, arms, "");
        self.out.push_str("\nend)()");
    }

    /// Emit match arms as an if/elseif chain with `return` in each branch.
    /// Used both by the IIFE wrapper (`emit_match_expr`) and the direct TCO
    /// path (`emit_tail_expr`).
    fn emit_match_arms(&mut self, var: &str, arms: &[ir::Branch], indent: &str) {
        let ind2 = format!("{}  ", indent);
        let ind3 = format!("{}    ", indent);
        for arm in arms {
            let cond = Self::pattern_cond(var, &arm.pattern);
            let binds = self.collect_binds_pure(var, &arm.pattern);
            let always_matches = cond == "true";
            let has_guard = arm.guard.is_some();

            self.out.push_str(&format!("{}do\n", indent));
            for (lhs, rhs) in &binds {
                self.out.push_str(&format!("{}  local {} = {}\n", indent, lhs, rhs));
            }
            if always_matches && !has_guard {
                self.out.push_str(&format!("{}  ", indent));
                self.emit_tail_expr(&arm.body, &ind2);
                self.out.push('\n');
            } else {
                self.out.push_str(&format!("{}  if ", indent));
                if !always_matches {
                    self.out.push_str(&cond);
                }
                if let Some(guard) = &arm.guard {
                    if !always_matches {
                        self.out.push_str(" and ");
                    }
                    self.emit_expr(guard);
                }
                self.out.push_str(" then\n");
                self.out.push_str(&format!("{}    ", indent));
                self.emit_tail_expr(&arm.body, &ind3);
                self.out.push_str(&format!("\n{}  end\n", indent));
            }
            self.out.push_str(&format!("{}end\n", indent));
        }
        self.out.push_str(&format!("{}error(\"incomplete match\")", indent));
    }

    // ── Pattern helpers ───────────────────────────────────────────────────────

    /// Returns a Lua boolean expression testing whether `var` matches `pat`.
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
                    format!("{} == {}", var, s)
                }
                ir::Lit::Str(s) => {
                    format!("{} == \"{}\"", var, s.replace('"', "\\\""))
                }
                ir::Lit::Bool(b) => format!("{} == {}", var, b),
            },
            ir::Pat::Tag(name, payload) => {
                let tag = format!("{}._tag == \"{}\"", var, name);
                match payload {
                    None => tag,
                    Some(p) => {
                        // Wrapped value lives at var._0
                        let inner = Self::pattern_cond(&format!("{}._0", var), p);
                        if inner == "true" {
                            tag
                        } else {
                            format!("({}) and ({})", tag, inner)
                        }
                    }
                }
            }
            ir::Pat::Record { fields, .. } => {
                let conds: Vec<String> = fields
                    .iter()
                    .filter_map(|(name, pat_opt)| {
                        pat_opt.as_ref().and_then(|p| {
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
                    conds.join(" and ")
                }
            }
            ir::Pat::List { elems, rest } => {
                let mut conds = vec![format!("type({}) == \"table\"", var)];
                if rest.is_none() {
                    conds.push(format!("#{} == {}", var, elems.len()));
                } else if !elems.is_empty() {
                    conds.push(format!("#{} >= {}", var, elems.len()));
                }
                for (i, elem) in elems.iter().enumerate() {
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
    fn collect_binds_pure(&mut self, var: &str, pat: &ir::Pat) -> Vec<(String, String)> {
        let mut out = Vec::new();
        self.collect_binds(var, pat, &mut out);
        out
    }

    fn collect_binds(&mut self, var: &str, pat: &ir::Pat, out: &mut Vec<(String, String)>) {
        match pat {
            ir::Pat::Wild | ir::Pat::Lit(_) => {}
            ir::Pat::Var(name) => out.push((lua_ident(name).into_owned(), var.to_string())),
            ir::Pat::Tag(name, payload) => {
                if let Some(p) = payload {
                    // All non-unit variants wrap their value at _0
                    if self.variant_env.lookup(name).is_some_and(|i| i.wraps.is_some()) {
                        self.collect_binds(&format!("{}._0", var), p, out);
                    } else {
                        self.collect_binds(var, p, out);
                    }
                }
            }
            ir::Pat::Record { fields, rest } => {
                for (name, pat_opt) in fields {
                    let field_expr = format!("{}.{}", var, name);
                    if let Some(p) = pat_opt {
                        self.collect_binds(&field_expr, p, out);
                    } else {
                        out.push((lua_ident(name).into_owned(), field_expr));
                    }
                }
                if let Some(Some(rest_name)) = rest {
                    self.needs_omit = true;
                    let excluded: Vec<_> = fields
                        .iter()
                        .map(|(name, _)| format!("\"{}\"", name))
                        .collect();
                    out.push((
                        rest_name.clone(),
                        format!("_omit({}, {{{}}})", var, excluded.join(", ")),
                    ));
                }
            }
            ir::Pat::List { elems, rest } => {
                for (i, elem) in elems.iter().enumerate() {
                    self.collect_binds(&format!("{}[{}]", var, i + 1), elem, out);
                }
                if let Some(Some(rest_name)) = rest {
                    self.needs_slice = true;
                    out.push((
                        lua_ident(rest_name).into_owned(),
                        format!("_slice({}, {})", var, elems.len() + 1),
                    ));
                }
            }
        }
    }
}
