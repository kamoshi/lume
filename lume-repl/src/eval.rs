use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use lume_core::types;

use crate::compile::{collect_new_dep_modules, compile_repl, is_module_import, strip_pub_export};
use crate::helper::{refresh_completions, DIM, RESET};

pub(crate) const SHOW_HELPER: &str = concat!(
    "function _show(x)\n",
    "  local t = type(x)\n",
    "  if t == \"string\" then return '\"' .. x .. '\"' end\n",
    "  if t == \"number\" then\n",
    "    if x == math.floor(x) then return tostring(math.floor(x)) else return tostring(x) end\n",
    "  end\n",
    "  if t == \"boolean\" then return x and \"true\" or \"false\" end\n",
    "  if t == \"nil\" then return \"()\" end\n",
    "  if t == \"table\" then\n",
    "    if x._tag ~= nil then\n",
    "      local parts = {}\n",
    "      for k, v in pairs(x) do\n",
    "        if k ~= \"_tag\" then parts[#parts+1] = _show(v) end\n",
    "      end\n",
    "      if #parts == 0 then return x._tag end\n",
    "      return x._tag .. \" \" .. parts[1]\n",
    "    end\n",
    "    local n = 0\n",
    "    for _ in pairs(x) do n = n + 1 end\n",
    "    local is_list = true\n",
    "    for k, _ in pairs(x) do\n",
    "      if type(k) ~= \"number\" then is_list = false; break end\n",
    "    end\n",
    "    if is_list then\n",
    "      local parts = {}\n",
    "      for _, v in ipairs(x) do parts[#parts+1] = _show(v) end\n",
    "      return \"[\" .. table.concat(parts, \", \") .. \"]\"\n",
    "    end\n",
    "    local parts = {}\n",
    "    for k, v in pairs(x) do parts[#parts+1] = k .. \": \" .. _show(v) end\n",
    "    table.sort(parts)\n",
    "    if #parts == 0 then return \"{}\" end\n",
    "    return \"{ \" .. table.concat(parts, \", \") .. \" }\"\n",
    "  end\n",
    "  return tostring(x)\n",
    "end\n",
);

pub(crate) enum EvalAction {
    None,
    ToggleVi,
}

pub(crate) fn eval_input(
    input: &str,
    lua: &mlua::Lua,
    repl_env: &mlua::Table,
    defs: &mut String,
    completions: &Arc<RwLock<Vec<String>>>,
    base_dir: &Path,
    loaded_modules: &mut HashMap<PathBuf, String>,
) -> EvalAction {
    let trimmed = input.trim();

    if trimmed == ":vi" {
        return EvalAction::ToggleVi;
    }
    if let Some(expr) = trimmed.strip_prefix(":type ").or_else(|| trimmed.strip_prefix(":t ")) {
        type_of(expr.trim(), defs, base_dir);
        return EvalAction::None;
    }
    if trimmed == ":type" || trimmed == ":t" {
        eprintln!("  usage: :type <expression>");
        return EvalAction::None;
    }
    if let Some(name) = trimmed.strip_prefix(":kind ").or_else(|| trimmed.strip_prefix(":k ")) {
        kind_of(name.trim(), defs, base_dir);
        return EvalAction::None;
    }
    if trimmed == ":kind" || trimmed == ":k" {
        eprintln!("  usage: :kind <TypeName>");
        return EvalAction::None;
    }

    let first_line = input.lines().find(|l| !l.trim().is_empty()).unwrap_or(input);
    let is_definition = first_line.starts_with("let ")
        || first_line.starts_with("type ")
        || first_line.starts_with("trait ")
        || first_line.starts_with("use ");

    if is_definition {
        // Ensure any `use` imports are compiled and available in Lua globals.
        if let Err(e) = ensure_imports_loaded(input, defs, base_dir, loaded_modules, lua) {
            eprintln!("  {e}");
            return EvalAction::None;
        }

        match compile_repl(defs, input, base_dir, loaded_modules) {
            Ok(lua_src) => {
                match lua.load(&lua_src).set_name("repl").set_environment(repl_env.clone()).exec() {
                    Ok(()) => {
                        // Module imports must stay before items; prepend them so
                        // the accumulated `defs` remains in canonical order.
                        if is_module_import(input) {
                            let entry = if input.ends_with('\n') {
                                input.to_string()
                            } else {
                                format!("{}\n", input)
                            };
                            *defs = format!("{}{}", entry, *defs);
                        } else {
                            if !defs.ends_with('\n') {
                                defs.push('\n');
                            }
                            defs.push_str(input);
                            defs.push('\n');
                        }
                        refresh_completions(defs, completions);
                    }
                    Err(e) => eprintln!("  runtime error: {e}"),
                }
            }
            Err(e) => eprintln!("  {e}"),
        }
    } else {
        let new_src = format!("let _repl_result = {}\n", input);
        match compile_repl(defs, &new_src, base_dir, loaded_modules) {
            Ok(lua_src) => {
                let chunk = format!(
                    "{}\nif _repl_result ~= nil then print(_show(_repl_result)) end",
                    lua_src
                );
                match lua.load(&chunk).set_name("repl").set_environment(repl_env.clone()).exec() {
                    Ok(()) => {}
                    Err(e) => eprintln!("  runtime error: {e}"),
                }
            }
            Err(e) => eprintln!("  {e}"),
        }
    }

    EvalAction::None
}

/// Parse `defs + input` to extract `use` declarations, then compile and load
/// any dep modules that aren't already in `loaded_modules` into Lua globals.
fn ensure_imports_loaded(
    input: &str,
    defs: &str,
    base_dir: &Path,
    loaded_modules: &mut HashMap<PathBuf, String>,
    lua: &mlua::Lua,
) -> Result<(), String> {
    use lume_core::lexer::Lexer;
    use lume_core::parser;

    // Mirror the ordering logic from compile_repl: module imports must precede items.
    let src = if is_module_import(input) {
        let sep = if input.ends_with('\n') { "" } else { "\n" };
        format!("{}{}{}pub {{}}\n", input, sep, defs)
    } else {
        let sep = if defs.is_empty() || defs.ends_with('\n') { "" } else { "\n" };
        format!("{}{}{}pub {{}}\n", defs, sep, input)
    };

    let tokens = Lexer::new(&src)
        .tokenize()
        .map_err(|e| format!("parse error: {e}"))?;
    let program = parser::parse_program(&tokens)
        .map_err(|e| format!("parse error: {e}"))?;

    if program.uses.is_empty() {
        return Ok(());
    }

    let new_deps = collect_new_dep_modules(&program.uses, base_dir, loaded_modules)?;

    if new_deps.lua_src.is_empty() {
        return Ok(());
    }

    // Load dep modules into Lua globals (no set_environment so they're
    // accessible from repl_env via its __index = _G metatable).
    lua.load(&new_deps.lua_src)
        .set_name("deps")
        .exec()
        .map_err(|e| format!("dep load error: {e}"))?;

    for (canonical, var_name) in new_deps.mods {
        loaded_modules.insert(canonical, var_name);
    }

    Ok(())
}

/// Print the inferred type of `expr` given `defs` as context.
pub(crate) fn type_of(expr: &str, defs: &str, base_dir: &Path) {
    use lume_core::lexer::Lexer;
    use lume_core::parser;

    let sep = if defs.is_empty() || defs.ends_with('\n') { "" } else { "\n" };
    let src = format!("{}{}let _repl_type = {}\n", defs, sep, expr);

    let tokens = match Lexer::new(&src).tokenize() {
        Ok(t) => t,
        Err(e) => { eprintln!("  parse error: {e}"); return; }
    };
    let program = match parser::parse_program(&tokens) {
        Ok(p) => p,
        Err(e) => { eprintln!("  parse error: {e}"); return; }
    };

    match types::infer::elaborate_with_env(&program, Some(base_dir)) {
        Ok((_, type_env, _)) => {
            match type_env.lookup("_repl_type") {
                Some(scheme) => println!("  {expr} :{DIM} {scheme}{RESET}"),
                None => eprintln!("  (could not determine type)"),
            }
        }
        Err(e) => eprintln!("  type error: {e}"),
    }
}

/// Print the kind of a type expression.
///
/// Parses the accumulated REPL definitions to build the arity environment,
/// then parses `expr` as a type and computes its kind using star notation
/// (e.g., `Maybe : * -> *`, `Box Num : *`, `Result : * -> * -> *`).
pub(crate) fn kind_of(expr: &str, defs: &str, base_dir: &Path) {
    use lume_core::ast::Type;
    use lume_core::lexer::Lexer;
    use lume_core::parser;
    use lume_core::types::infer::{build_arity_env, ArityEnv};

    // Build the arity env from the accumulated defs.
    let src = if defs.is_empty() { "let _x = 0\n".to_string() } else {
        let sep = if defs.ends_with('\n') { "" } else { "\n" };
        format!("{}{}", defs, sep)
    };

    let tokens = match Lexer::new(&src).tokenize() {
        Ok(t) => t,
        Err(e) => { eprintln!("  parse error: {e}"); return; }
    };
    let program = match parser::parse_program(&tokens) {
        Ok(p) => p,
        Err(e) => { eprintln!("  parse error: {e}"); return; }
    };

    let _ = base_dir;
    let arity_env = build_arity_env(&program.items);

    // Parse `expr` as a type expression (wrap in a dummy binding so the lexer
    // and parser see a valid program; we only need the type tokens).
    let type_src = format!("let _k : {} = 0\n", expr);
    let type_tokens = match Lexer::new(&type_src).tokenize() {
        Ok(t) => t,
        Err(e) => { eprintln!("  parse error: {e}"); return; }
    };
    // Find the `:` token and parse the type that follows it.
    // Token layout: `let` `_k` `:` <type tokens> `=` `0`
    // We can skip to after `:` and call parse_type.
    let colon_pos = type_tokens.iter().position(|t| {
        matches!(t.token, lume_core::lexer::Token::Colon)
    });
    let type_start = match colon_pos {
        Some(pos) => pos + 1,
        None => { eprintln!("  internal error: could not locate type in dummy program"); return; }
    };

    let (_, parsed_ty) = match parser::parse_type(&type_tokens[type_start..]) {
        Ok(r) => r,
        Err(e) => { eprintln!("  parse error: {e}"); return; }
    };

    // Recursively compute the remaining arity of a Type node.
    fn kind_of_ty(ty: &Type, env: &ArityEnv) -> Result<usize, String> {
        match ty {
            Type::Constructor(name) => {
                env.get(name.as_str())
                    .copied()
                    .ok_or_else(|| format!("unknown type '{name}'"))
            }
            Type::Var(_) => Ok(0), // type variables have kind *
            Type::App { callee, arg } => {
                let callee_arity = kind_of_ty(callee, env)?;
                let arg_arity = kind_of_ty(arg, env)?;
                if arg_arity != 0 {
                    return Err(format!(
                        "kind mismatch: argument has kind '{}', expected '*'",
                        arity_to_kind_string(arg_arity)
                    ));
                }
                if callee_arity == 0 {
                    return Err(format!(
                        "type '{}' is fully applied (kind *) and cannot be applied further",
                        callee
                    ));
                }
                Ok(callee_arity - 1)
            }
            Type::Func { .. } => Ok(0), // function types have kind *
            Type::Record(_) => Ok(0),   // record types have kind *
        }
    }

    match kind_of_ty(&parsed_ty, &arity_env) {
        Ok(arity) => {
            let kind = arity_to_kind_string(arity);
            println!("  {expr} :{DIM} {kind}{RESET}");
        }
        Err(e) => eprintln!("  {e}"),
    }
}

/// Convert an arity number to a star-notation kind string.
/// 0 → `*`, 1 → `* -> *`, 2 → `* -> * -> *`, etc.
fn arity_to_kind_string(arity: usize) -> String {
    if arity == 0 {
        return "*".to_string();
    }
    let mut parts: Vec<&str> = Vec::with_capacity(arity + 1);
    for _ in 0..=arity {
        parts.push("*");
    }
    parts.join(" -> ")
}

/// Load a Lume file into the REPL Lua environment.
///
/// Returns the file's source (with the `pub` export stripped) to use as the
/// initial `defs` accumulator so subsequent type-checks see the file's bindings.
pub(crate) fn load_file_into_repl(
    path: &str,
    lua: &mlua::Lua,
    repl_env: &mlua::Table,
    loaded_modules: &mut HashMap<PathBuf, String>,
) -> Result<String, String> {
    let file_path = std::path::Path::new(path)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let base_dir = file_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));

    let file_src = std::fs::read_to_string(&file_path).map_err(|e| e.to_string())?;
    let defs_src = strip_pub_export(&file_src).to_string();

    // Load any imports the file uses.
    ensure_imports_loaded(&defs_src, "", base_dir, loaded_modules, lua)?;

    let lua_src = compile_repl("", &defs_src, base_dir, loaded_modules)?;

    lua.load(&lua_src)
        .set_name(path)
        .set_environment(repl_env.clone())
        .exec()
        .map_err(|e| format!("runtime error: {e}"))?;

    Ok(defs_src)
}
