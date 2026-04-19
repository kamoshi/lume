use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lume_core::ast::TopItem;
use lume_core::bundle;
use lume_core::codegen;
use lume_core::lower;
use lume_core::types;

/// Type-check and lower a bundle to IR modules.
pub(crate) fn lower_bundle(
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

    {
        let mut scratch = lume_core::types::Subst::new();
        let (_, builtin_variants) = types::infer::builtin_env(&mut scratch);
        for (name, info) in builtin_variants.all() {
            global.variants.entry(name.clone()).or_insert_with(|| info.clone());
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
                            module_var: if is_local { None } else { e.module_var.clone() },
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
        let (node_types, type_env, resolved_trait_methods) =
            match types::infer::elaborate_with_env(&m.program, module_path) {
                Ok((nt, env, _, rtm)) => (nt, env, rtm),
                Err(e) => {
                    eprintln!("{}: type error: {e}", m.canonical.display());
                    return None;
                }
            };
        let ir_mod = lower::lower(m.program.clone(), &node_types, &type_env, &local_global, &resolved_trait_methods);
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

/// Return true if `src` (trimmed) is a module-import declaration
/// (`use <binding> = "path"`) as opposed to a trait-impl (`use T in Ty { … }`).
pub(crate) fn is_module_import(src: &str) -> bool {
    use lume_core::lexer::{Lexer, Token};
    let trimmed = src.trim_start();
    if !trimmed.starts_with("use ") {
        return false;
    }
    let tokens = match Lexer::new(trimmed).tokenize() {
        Ok(t) => t,
        Err(_) => return false,
    };
    // Skip the `use` token and scan until we hit `=` (import) or `in` (impl).
    let mut i = 1usize;
    while i < tokens.len() {
        match &tokens[i].token {
            Token::Equal => return true,
            Token::In | Token::Eof => return false,
            _ => i += 1,
        }
    }
    false
}

/// New dependency modules collected for the REPL.
pub(crate) struct NewDeps {
    /// `(canonical_path, lua_global_var_name)` for each newly-loaded module.
    pub mods: Vec<(PathBuf, String)>,
    /// Lua source that assigns each new module to its global variable.
    /// Execute this *without* `set_environment` so the vars live in globals.
    pub lua_src: String,
}

/// Collect and compile any imported modules not yet present in `loaded`.
///
/// Returns the Lua to load into globals plus the `(canonical, var)` pairs so
/// the caller can update its `loaded_modules` map.
pub(crate) fn collect_new_dep_modules(
    uses: &[lume_core::ast::UseDecl],
    base_dir: &Path,
    loaded: &HashMap<PathBuf, String>,
) -> Result<NewDeps, String> {
    use lume_core::loader::{resolve_path, stdlib_path, stdlib_source};

    // Gather full transitive dep bundles for all use declarations.
    let mut all_bundle: Vec<bundle::BundleModule> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for u in uses {
        let canonical = if stdlib_source(&u.path).is_some() {
            stdlib_path(&u.path)
        } else {
            resolve_path(&u.path, base_dir)
                .map_err(|e| format!("import error: {e}"))?
        };

        let dep_bundle = bundle::collect_dep(&canonical)?;
        for m in dep_bundle {
            if seen.insert(m.canonical.clone()) {
                all_bundle.push(m);
            }
        }
    }

    // Check whether any of these modules are new.
    let has_new = all_bundle.iter().any(|m| !loaded.contains_key(&m.canonical));
    if !has_new {
        return Ok(NewDeps { mods: vec![], lua_src: String::new() });
    }

    // Type-check and lower the full dep chain (needed for cross-module trait dispatch).
    let (ir_modules, variant_env) =
        lower_bundle(&all_bundle).ok_or_else(|| "dep compilation failed".to_string())?;

    // Build a complete module_vars map: already-loaded + new.
    let mut module_vars: HashMap<PathBuf, String> = loaded.clone();
    for ir_mod in &ir_modules {
        module_vars
            .entry(ir_mod.canonical.clone())
            .or_insert_with(|| ir_mod.var.clone());
    }

    // Collect references to new IrModules in order.
    let new_ir_mods: Vec<&codegen::IrModule> = ir_modules
        .iter()
        .filter(|m| !loaded.contains_key(&m.canonical))
        .collect();

    let mods: Vec<(PathBuf, String)> = new_ir_mods
        .iter()
        .map(|m| (m.canonical.clone(), m.var.clone()))
        .collect();

    let lua_src = codegen::lua::emit_dep_modules(&new_ir_mods, module_vars, variant_env);

    Ok(NewDeps { mods, lua_src })
}

/// Compile a new REPL input to Lua.
///
/// `defs` is the accumulated source from previous evals (used for type
/// checking only). `new_src` is the input being added now. Returns bare Lua
/// assignments for only the new bindings — no preamble, no `local`, no
/// `return`.
///
/// `base_dir` is the working directory used to resolve relative `use` paths
/// and for the type-checker to load imports.  `module_vars` maps canonical
/// dep paths to their Lua global var names so imports emit correct bindings.
pub(crate) fn compile_repl(
    defs: &str,
    new_src: &str,
    base_dir: &Path,
    module_vars: &HashMap<PathBuf, String>,
) -> Result<String, String> {
    use lume_core::lexer::Lexer;
    use lume_core::parser;

    // Count how many IR-emitting items are already in defs so we can skip
    // them in emission. TraitDef and TypeDef produce no IR; everything else
    // (Binding, BindingGroup, ImplDef) produces exactly one Decl each.
    let defs_ir_count = if defs.is_empty() {
        0
    } else {
        let toks = Lexer::new(defs)
            .tokenize()
            .map_err(|e| format!("parse error: {e}"))?;
        parser::parse_program(&toks)
            .map(|p| {
                p.items
                    .iter()
                    .filter(|i| {
                        matches!(
                            i,
                            TopItem::Binding(_)
                                | TopItem::BindingGroup(_)
                                | TopItem::ImplDef(_)
                        )
                    })
                    .count()
            })
            .unwrap_or(0)
    };

    // Use declarations must precede items in Lume source.  When new_src is a
    // module import (use X = "…"), prepend it so it stays before any `let`/
    // `type` bindings already in defs.  For everything else, append as usual.
    let full_src = if is_module_import(new_src) {
        let sep = if new_src.ends_with('\n') { "" } else { "\n" };
        format!("{}{}{}", new_src, sep, defs)
    } else {
        let sep = if defs.is_empty() || defs.ends_with('\n') { "" } else { "\n" };
        format!("{}{}{}", defs, sep, new_src)
    };

    let tokens = Lexer::new(&full_src)
        .tokenize()
        .map_err(|e| format!("parse error: {e}"))?;
    let program = parser::parse_program(&tokens)
        .map_err(|e| format!("parse error: {e}"))?;

    // Pass base_dir so the type checker can resolve `use` imports.
    let (node_types, type_env, resolved_trait_methods) =
        types::infer::elaborate_with_env(&program, Some(base_dir))
            .map(|(nt, env, _, rtm)| (nt, env, rtm))
            .map_err(|e| format!("type error: {e}"))?;

    let mut global = lower::GlobalCtx {
        traits: HashMap::new(),
        impls: HashMap::new(),
        param_impls: Vec::new(),
        variants: HashMap::new(),
    };

    {
        let mut scratch = lume_core::types::Subst::new();
        let (_, builtin_variants) = types::infer::builtin_env(&mut scratch);
        for (name, info) in builtin_variants.all() {
            global.variants.insert(name.clone(), info.clone());
        }
    }

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
                        lower::ImplEntry { module_var: None, dict_ident: dict },
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

    let ir_mod = lower::lower(program, &node_types, &type_env, &global, &resolved_trait_methods);

    let mut variant_env = types::infer::VariantEnv::default();
    for (name, info) in &global.variants {
        variant_env.insert(name.clone(), info.clone());
    }

    // Use cwd/_repl.lume as the canonical path so relative imports in
    // emit_import resolve correctly against the working directory.
    let ir_module = codegen::IrModule {
        canonical: base_dir.join("_repl.lume"),
        module: ir_mod,
        var: "_repl".to_string(),
    };

    Ok(codegen::lua::emit_repl(&ir_module, variant_env, defs_ir_count, module_vars.clone()))
}

/// Strip the trailing `pub { ... }` from a Lume source file.
///
/// We tokenize to find the exact byte offset of the `pub` keyword, so string
/// literals containing the word "pub" are handled correctly.
pub(crate) fn strip_pub_export(src: &str) -> &str {
    use lume_core::lexer::{Lexer, Token};

    let tokens = match Lexer::new(src).tokenize() {
        Ok(t) => t,
        Err(_) => return src,
    };

    for tok in &tokens {
        if tok.token == Token::Pub {
            let offset = line_col_to_byte_offset(src, tok.span.line, tok.span.col);
            return src[..offset].trim_end();
        }
    }
    src.trim_end()
}

fn line_col_to_byte_offset(src: &str, line: usize, col: usize) -> usize {
    let mut cur_line = 1;
    let mut cur_col = 1;
    for (i, ch) in src.char_indices() {
        if cur_line == line && cur_col == col {
            return i;
        }
        if ch == '\n' {
            cur_line += 1;
            cur_col = 1;
        } else {
            cur_col += 1;
        }
    }
    src.len()
}
