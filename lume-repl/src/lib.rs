//! Lume runtime: file execution and interactive REPL backed by LuaJIT.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lume_core::ast::TopItem;
use lume_core::bundle;
use lume_core::codegen;
use lume_core::lower;
use lume_core::types;

// ── File execution ───────────────────────────────────────────────────────────

/// Compile a `.lume` file (and its dependencies) to Lua and execute via LuaJIT.
/// Returns `Ok(())` on success, or an error message on failure.
pub fn run(path: &str) -> Result<(), String> {
    let b = bundle::collect(Path::new(path))
        .map_err(|e| format!("{path}: {e}"))?;

    let (ir_modules, variant_env) = lower_bundle(&b)
        .ok_or_else(|| "compilation failed".to_string())?;

    let lua_src = codegen::lua::emit(&ir_modules, variant_env);

    let lua = mlua::Lua::new();
    lua.load(&lua_src)
        .exec()
        .map_err(|e| format!("{path}: runtime error: {e}"))
}

// ── REPL ─────────────────────────────────────────────────────────────────────

/// Launch an interactive REPL. Blocks until the user exits (Ctrl-D).
pub fn run_repl() {
    use rustyline::error::ReadlineError;
    use rustyline::DefaultEditor;

    let lua = mlua::Lua::new();

    // Pre-load the _show helper for pretty-printing.
    lua.load(SHOW_HELPER)
        .exec()
        .expect("failed to load _show");

    // Accumulated Lume source (bindings, types, traits, impls).
    let mut defs = String::new();
    // Accumulated Lua source that has been executed so far.
    let mut lua_history = String::new();

    let mut rl = DefaultEditor::new().expect("failed to initialise terminal");

    eprintln!("Lume REPL — type expressions or let-bindings. Ctrl-D to exit.");

    loop {
        let readline = rl.readline("λ ");
        match readline {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&line);
                eval_input(input, &lua, &mut defs, &mut lua_history);
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => {
                eprintln!("bye.");
                break;
            }
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }
}

fn eval_input(
    input: &str,
    lua: &mlua::Lua,
    defs: &mut String,
    lua_history: &mut String,
) {
    let is_definition = input.starts_with("let ")
        || input.starts_with("type ")
        || input.starts_with("trait ")
        || input.starts_with("use ");

    if is_definition {
        let src = format!("{}{}\npub {{}}\n", defs, input);
        match compile_repl(&src) {
            Ok(lua_src) => {
                let new_lua = strip_prefix(&lua_src, lua_history);
                let new_lua = strip_trailing_return(new_lua);
                match lua.load(new_lua).exec() {
                    Ok(()) => {
                        defs.push_str(input);
                        defs.push('\n');
                        *lua_history = remove_trailing_return(&lua_src);
                    }
                    Err(e) => eprintln!("  runtime error: {e}"),
                }
            }
            Err(e) => eprintln!("  {e}"),
        }
    } else {
        // Expression — evaluate and print.
        let src = format!(
            "{}let __repl_result = {}\npub {{ __repl_result }}\n",
            defs, input
        );
        match compile_repl(&src) {
            Ok(lua_src) => {
                let new_lua = strip_prefix(&lua_src, lua_history);
                let new_lua = strip_trailing_return(new_lua);
                let chunk = format!(
                    "{}\nif __repl_result ~= nil then print(_show(__repl_result)) end",
                    new_lua
                );
                match lua.load(chunk.as_str()).exec() {
                    Ok(()) => {}
                    Err(e) => eprintln!("  runtime error: {e}"),
                }
            }
            Err(e) => eprintln!("  {e}"),
        }
    }
}

// ── Shared compilation helpers ───────────────────────────────────────────────

/// Type-check and lower a bundle to IR modules.
fn lower_bundle(
    b: &[bundle::BundleModule],
) -> Option<(Vec<codegen::IrModule>, types::infer::VariantEnv)> {
    let mut global = lower::GlobalCtx {
        traits: HashMap::new(),
        impls: HashMap::new(),
        param_impls: Vec::new(),
        variants: HashMap::new(),
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
                            types::infer::VariantInfo {
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

    // Built-in variants.
    {
        let mut scratch = lume_core::types::Subst::new();
        let (_, builtin_variants) = types::infer::builtin_env(&mut scratch);
        for (name, info) in builtin_variants.all() {
            global
                .variants
                .entry(name.clone())
                .or_insert_with(|| info.clone());
        }
    }

    let mut ir_modules = Vec::new();
    for m in b.iter() {
        let local_global = lower::GlobalCtx {
            traits: global.traits.clone(),
            impls: global
                .impls
                .iter()
                .map(|(k, e)| {
                    let is_local = e.module_var.as_deref() == Some(&m.var);
                    (
                        k.clone(),
                        lower::ImplEntry {
                            module_var: if is_local {
                                None
                            } else {
                                e.module_var.clone()
                            },
                            dict_ident: e.dict_ident.clone(),
                        },
                    )
                })
                .collect(),
            param_impls: global
                .param_impls
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
        let (node_types, type_env) =
            match types::infer::elaborate_with_env(&m.program, module_path) {
                Ok((nt, env, _)) => (nt, env),
                Err(e) => {
                    eprintln!("{}: type error: {e}", m.canonical.display());
                    return None;
                }
            };
        let ir_mod =
            lower::lower(m.program.clone(), &node_types, &type_env, &local_global);
        ir_modules.push(codegen::IrModule {
            canonical: m.canonical.clone(),
            module: ir_mod,
            var: m.var.clone(),
        });
    }

    let mut variant_env = types::infer::VariantEnv::default();
    for (name, info) in global.variants {
        variant_env.insert(name, info);
    }
    Some((ir_modules, variant_env))
}

/// Compile Lume source to Lua (single module, no file I/O).
fn compile_repl(src: &str) -> Result<String, String> {
    use lume_core::lexer::Lexer;
    use lume_core::parser;

    let tokens = Lexer::new(src)
        .tokenize()
        .map_err(|e| format!("parse error: {e}"))?;
    let program = parser::parse_program(&tokens)
        .map_err(|e| format!("parse error: {e}"))?;

    let (node_types, type_env) = types::infer::elaborate_with_env(&program, None)
        .map(|(nt, env, _)| (nt, env))
        .map_err(|e| format!("type error: {e}"))?;

    let mut global = lower::GlobalCtx {
        traits: HashMap::new(),
        impls: HashMap::new(),
        param_impls: Vec::new(),
        variants: HashMap::new(),
    };

    // Register built-in variants.
    {
        let mut scratch = lume_core::types::Subst::new();
        let (_, builtin_variants) = types::infer::builtin_env(&mut scratch);
        for (name, info) in builtin_variants.all() {
            global.variants.insert(name.clone(), info.clone());
        }
    }

    // Collect traits, impls, and user types from the program.
    for item in &program.items {
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
                            module_var: None,
                            dict_ident: dict,
                        },
                    );
                } else {
                    global.param_impls.push(lower::ParamImplEntry {
                        trait_name: id.trait_name.clone(),
                        target_type: id.target_type.clone(),
                        constraints: id.impl_constraints.clone(),
                        module_var: None,
                        dict_ident: dict,
                    });
                }
            }
            TopItem::TypeDef(td) => {
                for variant in &td.variants {
                    global.variants.insert(
                        variant.name.clone(),
                        types::infer::VariantInfo {
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

    let ir_mod = lower::lower(program, &node_types, &type_env, &global);

    let mut variant_env = types::infer::VariantEnv::default();
    for (name, info) in &global.variants {
        variant_env.insert(name.clone(), info.clone());
    }

    let ir_module = codegen::IrModule {
        canonical: PathBuf::from("<repl>"),
        module: ir_mod,
        var: "__repl".to_string(),
    };

    Ok(codegen::lua::emit(&[ir_module], variant_env))
}

// ── String helpers ───────────────────────────────────────────────────────────

fn strip_prefix<'a>(full: &'a str, prefix: &str) -> &'a str {
    full.strip_prefix(prefix).unwrap_or(full)
}

fn strip_trailing_return(s: &str) -> &str {
    if let Some(idx) = s.rfind("\nreturn ") {
        &s[..idx]
    } else if s.starts_with("return ") {
        ""
    } else {
        s
    }
}

fn remove_trailing_return(s: &str) -> String {
    if let Some(idx) = s.rfind("\nreturn ") {
        s[..idx + 1].to_string()
    } else if s.starts_with("return ") {
        String::new()
    } else {
        s.to_string()
    }
}

// ── _show Lua helper ─────────────────────────────────────────────────────────

const SHOW_HELPER: &str = concat!(
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
