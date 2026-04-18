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
