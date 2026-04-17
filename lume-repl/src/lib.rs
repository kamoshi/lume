//! Lume runtime: file execution and interactive REPL backed by LuaJIT.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lume_core::ast::TopItem;
use lume_core::bundle;
use lume_core::codegen;
use lume_core::lower;
use lume_core::types;

// ── Syntax highlighting ───────────────────────────────────────────────────────

// ANSI escape helpers (no extra deps needed)
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const FG_CYAN: &str = "\x1b[36m";
const FG_YELLOW: &str = "\x1b[33m";
const FG_GREEN: &str = "\x1b[32m";
const FG_MAGENTA: &str = "\x1b[35m";
const FG_BLUE: &str = "\x1b[34m";
const FG_RED: &str = "\x1b[31m";

fn highlight_line(line: &str) -> String {
    use lume_core::lexer::{Lexer, Token};

    // REPL commands get special coloring
    if line.starts_with(':') {
        return format!("{FG_MAGENTA}{BOLD}{line}{RESET}");
    }

    // Tokenise; on failure return plain text
    let tokens = match Lexer::new(line).tokenize() {
        Ok(t) => t,
        Err(_) => return line.to_string(),
    };

    // Build a coloured string by walking token spans.
    // Each token carries a Span (col, len) in the original source.
    let mut out = String::with_capacity(line.len() + 64);
    let mut cursor = 0usize; // byte offset into `line`

    for spanned in &tokens {
        let tok = &spanned.token;
        if matches!(tok, Token::Eof) {
            break;
        }

        let span = &spanned.span;
        let col = span.col.saturating_sub(1); // 1-based → 0-based
        let len = span.len;

        // Append any gap between last token and this one (whitespace/etc.)
        if col > cursor {
            out.push_str(&line[cursor..col]);
        }
        let end = (col + len).min(line.len());
        let lexeme = &line[col..end];
        cursor = end;

        let colored = match tok {
            // Keywords
            Token::Let | Token::Pub | Token::Type | Token::Use
            | Token::If | Token::Then | Token::Else | Token::In
            | Token::Trait | Token::Match | Token::And | Token::Not => {
                format!("{FG_BLUE}{BOLD}{lexeme}{RESET}")
            }
            // Boolean literals
            Token::True | Token::False => {
                format!("{FG_CYAN}{lexeme}{RESET}")
            }
            // Type identifiers (PascalCase)
            Token::TypeIdent(_) => {
                format!("{FG_YELLOW}{lexeme}{RESET}")
            }
            // String literals
            Token::Text(_) => {
                format!("{FG_GREEN}{lexeme}{RESET}")
            }
            // Numeric literals
            Token::Number(_) => {
                format!("{FG_CYAN}{lexeme}{RESET}")
            }
            // Doc comments
            Token::DocComment(_) => {
                format!("{DIM}{lexeme}{RESET}")
            }
            // Pipe operators (key Lume operators)
            Token::Pipe | Token::ResultPipe => {
                format!("{FG_MAGENTA}{lexeme}{RESET}")
            }
            // Arrow / fat arrow
            Token::Arrow | Token::FatArrow => {
                format!("{FG_RED}{lexeme}{RESET}")
            }
            // Other operators
            Token::Plus | Token::Minus | Token::Star | Token::Slash
            | Token::EqEq | Token::BangEq | Token::Lt | Token::Gt
            | Token::LtEq | Token::GtEq | Token::Concat
            | Token::AmpAmp | Token::PipePipe | Token::Equal
            | Token::Colon | Token::Bar | Token::DotDot | Token::Dot => {
                format!("{DIM}{lexeme}{RESET}")
            }
            // Plain identifiers — no color (terminal default)
            Token::Ident(_) => lexeme.to_string(),
            // Punctuation — no color
            _ => lexeme.to_string(),
        };
        out.push_str(&colored);
    }

    // Append any remaining characters after the last token
    if cursor < line.len() {
        out.push_str(&line[cursor..]);
    }

    out
}

struct LumeHelper;

impl rustyline::highlight::Highlighter for LumeHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Owned(highlight_line(line))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: rustyline::highlight::CmdKind) -> bool {
        true // re-highlight on every keystroke
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Owned(format!("{FG_MAGENTA}{BOLD}{prompt}{RESET}"))
    }
}

impl rustyline::completion::Completer for LumeHelper {
    type Candidate = String;
}
impl rustyline::hint::Hinter for LumeHelper {
    type Hint = String;
}
impl rustyline::validate::Validator for LumeHelper {
    fn validate(
        &self,
        ctx: &mut rustyline::validate::ValidationContext,
    ) -> rustyline::Result<rustyline::validate::ValidationResult> {
        use lume_core::error::ParseErrorKind;
        use lume_core::lexer::Lexer;
        use lume_core::parser;

        let input = ctx.input();
        // Skip REPL commands — always valid immediately
        if input.starts_with(':') {
            return Ok(rustyline::validate::ValidationResult::Valid(None));
        }

        // Wrap in a dummy pub export so the parser sees a complete program
        let src = format!("{input}\npub {{}}\n");
        let result = Lexer::new(&src)
            .tokenize()
            .map_err(|_| ())
            .and_then(|tokens| parser::parse_program(&tokens).map_err(|_| ()));

        match result {
            Ok(_) => Ok(rustyline::validate::ValidationResult::Valid(None)),
            Err(_) => {
                // Re-run to get the actual error kind
                let src2 = format!("{input}\npub {{}}\n");
                let is_eof = Lexer::new(&src2)
                    .tokenize()
                    .ok()
                    .and_then(|tokens| parser::parse_program(&tokens).err())
                    .map(|e| matches!(e.kind, ParseErrorKind::UnexpectedEof))
                    .unwrap_or(false);
                if is_eof {
                    Ok(rustyline::validate::ValidationResult::Incomplete)
                } else {
                    // Other parse errors: let readline accept the input anyway
                    // so eval_input can report the error with context
                    Ok(rustyline::validate::ValidationResult::Valid(None))
                }
            }
        }
    }
}
impl rustyline::Helper for LumeHelper {}

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
    use rustyline::{Config, Editor};

    let lua = mlua::Lua::new();

    // Pre-load the _show helper for pretty-printing.
    lua.load(SHOW_HELPER)
        .exec()
        .expect("failed to load _show");

    // Accumulated Lume source (bindings, types, traits, impls).
    let mut defs = String::new();
    // Accumulated Lua source that has been executed so far.
    let mut lua_history = String::new();

    let mut rl: Editor<LumeHelper, _> = Editor::with_config(Config::default())
        .expect("failed to initialise terminal");
    rl.set_helper(Some(LumeHelper));

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

/// Like `run_repl` but reads input from stdin line-by-line (for piping / scripting).
pub fn run_repl_stdin() {
    use std::io::{self, BufRead};

    let lua = mlua::Lua::new();
    lua.load(SHOW_HELPER).exec().expect("failed to load _show");

    let mut defs = String::new();
    let mut lua_history = String::new();

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => { eprintln!("  read error: {e}"); break; }
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        eval_input(input, &lua, &mut defs, &mut lua_history);
    }
}


fn type_of(expr: &str, defs: &str) {
    use lume_core::lexer::Lexer;
    use lume_core::parser;

    let src = format!("{}let _repl_type = {}\n", defs, expr);

    let tokens = match Lexer::new(&src).tokenize() {
        Ok(t) => t,
        Err(e) => { eprintln!("  parse error: {e}"); return; }
    };
    let program = match parser::parse_program(&tokens) {
        Ok(p) => p,
        Err(e) => { eprintln!("  parse error: {e}"); return; }
    };

    match types::infer::elaborate_with_env(&program, None) {
        Ok((_, type_env, _)) => {
            match type_env.lookup("_repl_type") {
                Some(scheme) => println!("  {expr} : {scheme}"),
                None => eprintln!("  (could not determine type)"),
            }
        }
        Err(e) => eprintln!("  type error: {e}"),
    }
}

fn eval_input(
    input: &str,
    lua: &mlua::Lua,
    defs: &mut String,
    lua_history: &mut String,
) {
    // ── REPL commands ────────────────────────────────────────────────────────
    let trimmed = input.trim();
    if let Some(expr) = trimmed.strip_prefix(":type ").or_else(|| trimmed.strip_prefix(":t ")) {
        type_of(expr.trim(), defs);
        return;
    }
    if trimmed == ":type" || trimmed == ":t" {
        eprintln!("  usage: :type <expression>");
        return;
    }

    let first_line = input.lines().find(|l| !l.trim().is_empty()).unwrap_or(input);
    let is_definition = first_line.starts_with("let ")
        || first_line.starts_with("type ")
        || first_line.starts_with("trait ")
        || first_line.starts_with("use ");

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
            "{}let _repl_result = {}\n",
            defs, input
        );
        match compile_repl(&src) {
            Ok(lua_src) => {
                let new_lua = strip_prefix(&lua_src, lua_history);
                let new_lua = strip_trailing_return(new_lua);
                let chunk = format!(
                    "{}\nif _repl_result ~= nil then print(_show(_repl_result)) end",
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
