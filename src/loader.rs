use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::Span;
use crate::lexer::Lexer;
use crate::parser;
use crate::types::{
    infer::{build_variant_env, builtin_env, Checker},
    Scheme,
};
use crate::types::error::{TypeError, TypeErrorAt};
use crate::ast::Program;

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

/// Loads, parses, and type-checks Lume source files, caching the result of
/// each module so it is only compiled once per build.
pub struct Loader {
    /// Canonical path → generalised export scheme.
    cache: HashMap<PathBuf, Scheme>,
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Loader {
    pub fn new() -> Self {
        Loader { cache: HashMap::new() }
    }

    /// Parse `src` (already read from disk) and return the AST.
    pub fn parse(src: &str) -> Result<Program, String> {
        let tokens = Lexer::new(src).tokenize().map_err(|e| e.to_string())?;
        parser::parse_program(&tokens).map_err(|e| e.to_string())
    }

    /// Load, parse, and type-check the module at `raw_path` (resolved relative
    /// to `base`).  Returns the generalised export scheme.  Results are cached
    /// so each module is compiled at most once.
    pub fn load(&mut self, raw_path: &str, base: &Path) -> Result<Scheme, TypeErrorAt> {
        let canonical = resolve_path(raw_path, base).map_err(|msg| {
            TypeErrorAt::new(TypeError::ImportError(msg), Span::default())
        })?;

        if let Some(scheme) = self.cache.get(&canonical).cloned() {
            return Ok(scheme);
        }

        let src = std::fs::read_to_string(&canonical).map_err(|e| {
            TypeErrorAt::new(
                TypeError::ImportError(format!(
                    "cannot read '{}': {}",
                    canonical.display(),
                    e
                )),
                Span::default(),
            )
        })?;

        let program = Self::parse(&src).map_err(|msg| {
            TypeErrorAt::new(TypeError::ImportError(msg), Span::default())
        })?;

        let scheme = self.check_and_generalise(&program, &canonical)?;
        self.cache.insert(canonical, scheme.clone());
        Ok(scheme)
    }

    /// Type-check `program` (located at `path`) and return its generalised
    /// export scheme.  Uses `self` to resolve any transitive imports.
    pub fn check_and_generalise(
        &mut self,
        program: &Program,
        path: &Path,
    ) -> Result<Scheme, TypeErrorAt> {
        let (env, mut var_env) = builtin_env();
        let prog_vars = build_variant_env(&program.items);
        var_env.merge(prog_vars);
        let mut checker = Checker::new(var_env);
        let export_ty = checker.check_program(program, env, Some(path), self)?;
        Ok(generalise_toplevel(&checker.subst, &export_ty))
    }
}

/// Generalise a fully-applied type as a top-level definition: every remaining
/// free variable becomes a quantified parameter (valid because the environment
/// is empty at module boundary).
pub fn generalise_toplevel(subst: &crate::types::Subst, ty: &crate::types::Ty) -> Scheme {
    use crate::types::{free_row_vars, free_type_vars};
    let ty = subst.apply(ty);
    Scheme {
        vars: free_type_vars(&ty).into_iter().collect(),
        row_vars: free_row_vars(&ty).into_iter().collect(),
        ty,
    }
}
