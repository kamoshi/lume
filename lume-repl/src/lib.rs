//! Lume runtime: file execution and interactive REPL backed by LuaJIT.

mod compile;
mod eval;
mod helper;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use lume_core::bundle;
use lume_core::codegen;

use compile::{compile_prelude_deps, lower_bundle};
use eval::{load_file_into_repl, EvalAction, SHOW_HELPER};
use helper::{refresh_completions, LumeHelper};

// ── File execution ────────────────────────────────────────────────────────────

/// Compile a `.lume` file (and its dependencies) to Lua and execute via LuaJIT.
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

// ── Interactive REPL ──────────────────────────────────────────────────────────

/// Launch an interactive REPL. Blocks until the user exits (Ctrl-D).
///
/// If `file` is provided, that Lume file is compiled and loaded into the REPL
/// environment before the interactive session begins.
pub fn run_repl(file: Option<&str>) {
    use rustyline::error::ReadlineError;
    use rustyline::{Config, Editor};

    let lua = mlua::Lua::new();
    lua.load(&codegen::lua::full_prelude()).exec().expect("failed to load prelude");
    lua.load(SHOW_HELPER).exec().expect("failed to load _show");

    let repl_env = lua.create_table().expect("failed to create repl env");
    let mt = lua.create_table().expect("failed to create repl env metatable");
    mt.set("__index", lua.globals()).expect("failed to set __index");
    repl_env.set_metatable(Some(mt)).expect("failed to set metatable");

    let defs_lock: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let mut defs = String::new();
    let base_dir: PathBuf = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut loaded_modules: HashMap<PathBuf, String> = HashMap::new();

    // Auto-load prelude (Functor, map, etc.) into the Lua runtime.
    match compile_prelude_deps(&loaded_modules) {
        Ok(deps) => {
            if !deps.lua_src.is_empty() {
                lua.load(&deps.lua_src).set_name("prelude_deps").exec()
                    .expect("failed to load prelude deps");
                for (canonical, var_name) in deps.mods {
                    loaded_modules.insert(canonical, var_name);
                }
            }
        }
        Err(e) => eprintln!("warning: prelude load failed: {e}"),
    }

    if let Some(path) = file {
        match load_file_into_repl(path, &lua, &repl_env, &mut loaded_modules) {
            Ok(src) => {
                eprintln!("Loaded {path}.");
                *defs_lock.write().unwrap() = src.clone();
                defs = src;
            }
            Err(e) => { eprintln!("{path}: {e}"); return; }
        }
    }

    let helper = LumeHelper::new(Arc::clone(&defs_lock), base_dir.clone());
    let completions = helper.completions_handle();

    if !defs.is_empty() {
        refresh_completions(&defs, &completions);
    }

    let mut rl: Editor<LumeHelper, _> = Editor::with_config(Config::default())
        .expect("failed to initialise terminal");
    rl.set_helper(Some(helper));

    let mut vi_mode = false;

    eprintln!("Lume REPL — type expressions or let-bindings. Ctrl-D to exit.");

    loop {
        let readline = rl.readline("λ ");
        match readline {
            Ok(line) => {
                let input = line.trim();
                if input.is_empty() { continue; }
                let _ = rl.add_history_entry(&line);
                let prev_len = defs.len();
                match eval::eval_input(input, &lua, &repl_env, &mut defs, &completions, &base_dir, &mut loaded_modules) {
                    EvalAction::ToggleVi => {
                        use rustyline::config::{Configurer, EditMode};
                        vi_mode = !vi_mode;
                        if vi_mode {
                            rl.set_edit_mode(EditMode::Vi);
                            eprintln!("  vi mode on");
                        } else {
                            rl.set_edit_mode(EditMode::Emacs);
                            eprintln!("  vi mode off");
                        }
                    }
                    EvalAction::None => {}
                }
                if defs.len() != prev_len {
                    *defs_lock.write().unwrap() = defs.clone();
                }
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => { eprintln!("bye."); break; }
            Err(e) => { eprintln!("readline error: {e}"); break; }
        }
    }
}

/// Like `run_repl` but reads input from stdin line-by-line (for piping / scripting).
pub fn run_repl_stdin(file: Option<&str>) {
    use std::io::{self, BufRead};

    let lua = mlua::Lua::new();
    lua.load(&codegen::lua::full_prelude()).exec().expect("failed to load prelude");
    lua.load(SHOW_HELPER).exec().expect("failed to load _show");

    let repl_env = lua.create_table().expect("failed to create repl env");
    let mt = lua.create_table().expect("failed to create repl env metatable");
    mt.set("__index", lua.globals()).expect("failed to set __index");
    repl_env.set_metatable(Some(mt)).expect("failed to set metatable");

    let mut defs = String::new();
    let base_dir: PathBuf = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut loaded_modules: HashMap<PathBuf, String> = HashMap::new();

    // Auto-load prelude (Functor, map, etc.) into the Lua runtime.
    match compile_prelude_deps(&loaded_modules) {
        Ok(deps) => {
            if !deps.lua_src.is_empty() {
                lua.load(&deps.lua_src).set_name("prelude_deps").exec()
                    .expect("failed to load prelude deps");
                for (canonical, var_name) in deps.mods {
                    loaded_modules.insert(canonical, var_name);
                }
            }
        }
        Err(e) => eprintln!("warning: prelude load failed: {e}"),
    }

    if let Some(path) = file {
        match load_file_into_repl(path, &lua, &repl_env, &mut loaded_modules) {
            Ok(src) => defs = src,
            Err(e) => { eprintln!("{path}: {e}"); return; }
        }
    }

    let defs_lock: Arc<RwLock<String>> = Arc::new(RwLock::new(defs.clone()));
    let helper = LumeHelper::new(Arc::clone(&defs_lock), base_dir.clone());
    let completions = helper.completions_handle();
    if !defs.is_empty() {
        refresh_completions(&defs, &completions);
    }

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => { eprintln!("  read error: {e}"); break; }
        };
        let input = line.trim();
        if input.is_empty() { continue; }
        eval::eval_input(input, &lua, &repl_env, &mut defs, &completions, &base_dir, &mut loaded_modules);
    }
}
