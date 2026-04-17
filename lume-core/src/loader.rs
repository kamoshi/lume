use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ast::{Program, TopItem, TraitDef, Type};
use crate::error::Span;
use crate::lexer::Lexer;
use crate::parser;
use crate::types::error::{TypeError, TypeErrorAt};
use crate::types::{
    infer::{build_variant_env, builtin_env, Checker, VariantEnv},
    Scheme, Subst,
};

// ── Embedded standard library ─────────────────────────────────────────────────

/// Every built-in stdlib module path, sorted alphabetically.
/// This is the single source of truth for tooling (LSP completions, WASM, etc.).
pub const STDLIB_MODULES: &[&str] = &[
    "lume:list",
    "lume:math",
    "lume:maybe",
    "lume:result",
    "lume:text",
];

/// Discriminates the kind of path inside a `use` declaration.
pub enum UsePathKind {
    /// `"lume:<name>"` - an embedded stdlib module.
    Stdlib,
    /// A filesystem path, e.g. `"./utils"` or `"../shared"`.
    /// Requires filesystem access to produce suggestions; the WASM backend
    /// returns no completions for this variant.
    File,
}

/// Context produced when the cursor is inside the path string of a `use`
/// declaration.
pub struct UsePathContext {
    /// Whether this is a stdlib (`lume:`) or filesystem path.
    pub kind: UsePathKind,
    /// Text typed after the scheme separator:
    /// - `Stdlib`: text after `lume:`, e.g. `"ma"` for `"lume:ma"`
    /// - `File`:   the entire string content so far, e.g. `"./fo"`
    pub prefix: String,
    /// Byte offset within the current line where `prefix` starts.
    /// Use this to build the replacement range for a completion item.
    pub prefix_col: usize,
}

/// If the text from the start of the current line **up to the cursor** is
/// inside the path string of a `use` declaration, returns the context;
/// otherwise returns `None`.
///
/// Handles both `use ident = "…"` and `use { fields } = "…"` syntax.
pub fn use_path_context(line_up_to_cursor: &str) -> Option<UsePathContext> {
    let bytes = line_up_to_cursor.as_bytes();
    let mut in_string = false;
    let mut quote_col = 0usize;
    let mut string_content = String::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if !in_string {
            if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
                return None; // line comment - no completions
            }
            if b == b'"' {
                in_string = true;
                quote_col = i;
                string_content.clear();
            }
        } else {
            match b {
                b'\\' => {
                    i += 1;
                } // skip escaped character
                b'"' => {
                    // String closed before cursor - keep scanning (edge case:
                    // multiple strings on one line).
                    in_string = false;
                    string_content.clear();
                }
                _ => string_content.push(b as char),
            }
        }
        i += 1;
    }

    if !in_string {
        return None;
    }

    // The text before the opening quote must look like `use <binding> =`.
    let before = line_up_to_cursor[..quote_col].trim();
    if !is_use_assignment(before) {
        return None;
    }

    if let Some(stripped) = string_content.strip_prefix("lume:") {
        Some(UsePathContext {
            kind: UsePathKind::Stdlib,
            prefix: stripped.to_string(),
            prefix_col: quote_col + 1 + 5, // byte after `"lume:`
        })
    } else {
        Some(UsePathContext {
            kind: UsePathKind::File,
            prefix: string_content,
            prefix_col: quote_col + 1, // byte right after `"`
        })
    }
}

/// Returns `true` if `s` (already trimmed) matches the pattern `use <binding> =`.
/// Accepts both `use ident =` and `use { … } =`.
fn is_use_assignment(s: &str) -> bool {
    let rest = match s.strip_prefix("use") {
        Some(r) if r.starts_with(|c: char| c.is_whitespace()) => r.trim_start(),
        _ => return false,
    };
    // After "use ", must have at least one non-"=" character followed by "=".
    !rest.is_empty() && rest.trim_end().ends_with('=')
}

/// Returns the source for a `lume:*` stdlib module, or `None` if the name is
/// not recognised.
///
/// The source files are embedded at compile time so no filesystem access is
/// needed at runtime (important for WASM and for reproducible builds).
pub fn stdlib_source(name: &str) -> Option<&'static str> {
    match name {
        "lume:list" => Some(include_str!("../../std/list.lume")),
        "lume:text" => Some(include_str!("../../std/text.lume")),
        "lume:math" => Some(include_str!("../../std/math.lume")),
        "lume:maybe" => Some(include_str!("../../std/maybe.lume")),
        "lume:result" => Some(include_str!("../../std/result.lume")),
        _ => None,
    }
}

/// A synthetic, stable `PathBuf` used as the cache key for an embedded stdlib
/// module.  It never exists on disk - it just needs to be unique per module.
pub fn stdlib_path(name: &str) -> PathBuf {
    PathBuf::from(format!("<{}>", name))
}

/// Resolves a raw import path (e.g. `"./math"` or `"./math.lume"`) relative
/// to `base` (the file doing the importing).
pub fn resolve_path(raw: &str, base: &Path) -> Result<PathBuf, String> {
    let with_ext = if raw.ends_with(".lume") {
        raw.to_string()
    } else {
        format!("{}.lume", raw)
    };
    let dir = if base.is_dir() {
        base.to_path_buf()
    } else {
        base.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };
    dir.join(with_ext)
        .canonicalize()
        .map_err(|e| format!("cannot resolve '{}': {}", raw, e))
}

/// Exported information from a compiled module.
#[derive(Clone)]
#[allow(clippy::type_complexity)]
pub struct ModuleExports {
    pub scheme: Scheme,
    pub variant_env: VariantEnv,
    pub trait_env: HashMap<String, TraitDef>,
    pub impl_env: HashMap<(String, String), String>,
    pub param_impl_env: Vec<(String, Type, Vec<(String, String)>)>,
}

/// Loads, parses, and type-checks Lume source files, caching the result of
/// each module so it is only compiled once per build.
pub struct Loader {
    /// Canonical path → compiled module exports.
    cache: HashMap<PathBuf, ModuleExports>,
    /// Canonical paths that are currently being loaded.  Used to detect and
    /// break import cycles, turning what would be infinite recursion into a
    /// clean `ImportError`.
    visiting: std::collections::HashSet<PathBuf>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Loader {
    pub fn new() -> Self {
        Loader {
            cache: HashMap::new(),
            visiting: std::collections::HashSet::new(),
        }
    }

    /// Parse `src` (already read from disk) and return the AST.
    pub fn parse(src: &str) -> Result<Program, String> {
        let tokens = Lexer::new(src).tokenize().map_err(|e| e.to_string())?;
        parser::parse_program(&tokens).map_err(|e| e.to_string())
    }

    /// Load, parse, and type-check the module at `raw_path` (resolved relative
    /// to `base`).  Returns the module's exports (scheme, variant env, trait env).
    /// Results are cached so each module is compiled at most once.
    ///
    /// Paths of the form `"lume:*"` are resolved against the embedded standard
    /// library instead of the filesystem.
    pub fn load(&mut self, raw_path: &str, base: &Path) -> Result<ModuleExports, TypeErrorAt> {
        // ── Embedded stdlib ───────────────────────────────────────────────────
        if let Some(src) = stdlib_source(raw_path) {
            let key = stdlib_path(raw_path);
            if let Some(exports) = self.cache.get(&key).cloned() {
                return Ok(exports);
            }
            let program = Self::parse(src)
                .map_err(|msg| TypeErrorAt::new(TypeError::ImportError(msg), Span::default()))?;
            // Stdlib modules have no on-disk path so pass the synthetic key as
            // the base; relative imports inside stdlib are not supported.
            let exports = self.check_and_generalise(&program, &key)?;
            self.cache.insert(key, exports.clone());
            return Ok(exports);
        }

        // ── Filesystem module ─────────────────────────────────────────────────
        let canonical = resolve_path(raw_path, base)
            .map_err(|msg| TypeErrorAt::new(TypeError::ImportError(msg), Span::default()))?;

        if let Some(exports) = self.cache.get(&canonical).cloned() {
            return Ok(exports);
        }

        // Detect import cycles: if we're already in the process of loading this
        // module, a circular dependency exists.  Return an error rather than
        // recursing infinitely.
        if self.visiting.contains(&canonical) {
            return Err(TypeErrorAt::new(
                TypeError::ImportError(format!("circular import: '{}'", canonical.display())),
                Span::default(),
            ));
        }

        let src = std::fs::read_to_string(&canonical).map_err(|e| {
            TypeErrorAt::new(
                TypeError::ImportError(format!("cannot read '{}': {}", canonical.display(), e)),
                Span::default(),
            )
        })?;

        let program = Self::parse(&src)
            .map_err(|msg| TypeErrorAt::new(TypeError::ImportError(msg), Span::default()))?;

        self.visiting.insert(canonical.clone());
        let exports = self.check_and_generalise(&program, &canonical)?;
        self.visiting.remove(&canonical);
        self.cache.insert(canonical, exports.clone());
        Ok(exports)
    }

    /// Type-check `program` (located at `path`) and return its exports:
    /// generalised scheme, locally-defined variant env, and locally-defined trait env.
    /// Uses `self` to resolve any transitive imports.
    pub fn check_and_generalise(
        &mut self,
        program: &Program,
        path: &Path,
    ) -> Result<ModuleExports, TypeErrorAt> {
        let mut subst = Subst::new();
        let (env, mut var_env) = builtin_env(&mut subst);
        let prog_vars = build_variant_env(&program.items);
        var_env.merge(prog_vars.clone());
        let mut checker = Checker::with_subst(var_env, subst);
        let export_ty = checker.check_program(program, env, Some(path), self)?;
        let scheme = generalise_toplevel(&checker.subst, &export_ty, &checker.constraint_map);
        // Collect locally-defined traits for export.
        let trait_env: HashMap<String, TraitDef> = program
            .items
            .iter()
            .filter_map(|item| {
                if let TopItem::TraitDef(td) = item {
                    Some((td.name.clone(), td.clone()))
                } else {
                    None
                }
            })
            .collect();
        // Replace "<local>" source markers with the actual module path so
        // downstream duplicate-impl detection can distinguish diamond imports
        // (same source) from independent duplicates (different sources).
        let path_str = path.to_string_lossy().to_string();
        let impl_env: HashMap<(String, String), String> = checker.impl_env
            .into_iter()
            .map(|(k, v)| {
                if v == "<local>" { (k, path_str.clone()) } else { (k, v) }
            })
            .collect();
        Ok(ModuleExports {
            scheme,
            variant_env: prog_vars,
            trait_env,
            impl_env,
            param_impl_env: checker.param_impl_env,
        })
    }
}

/// Generalise a fully-applied type as a top-level definition: every remaining
/// free variable becomes a quantified parameter (valid because the environment
/// is empty at module boundary).
pub fn generalise_toplevel(
    subst: &crate::types::Subst,
    ty: &crate::types::Ty,
    constraint_map: &[(String, crate::types::TyVar)],
) -> Scheme {
    use crate::types::{free_row_vars, free_type_vars, Ty};
    let ty = subst.apply(ty);
    let generalised: std::collections::HashSet<crate::types::TyVar> =
        free_type_vars(&ty).into_iter().collect();
    let mut seen = std::collections::HashSet::new();
    let constraints: Vec<(String, crate::types::TyVar)> = constraint_map
        .iter()
        .filter_map(|(trait_name, fresh_var)| {
            match subst.apply(&Ty::Var(*fresh_var)) {
                Ty::Var(v) if generalised.contains(&v) => {
                    let pair = (trait_name.clone(), v);
                    if seen.insert(pair.clone()) {
                        Some(pair)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        })
        .collect();
    Scheme {
        vars: free_type_vars(&ty).into_iter().collect(),
        row_vars: free_row_vars(&ty).into_iter().collect(),
        constraints,
        ty,
    }
}
