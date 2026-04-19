use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::Program;
use crate::loader::{prelude_path, resolve_path, stdlib_path, stdlib_source, Loader};

pub struct BundleModule {
    /// Canonical (absolute) path of this module.
    pub canonical: PathBuf,
    /// Parsed AST.
    pub program: Program,
    /// The local variable name used to hold this module's exports in the
    /// emitted output (e.g. `_mod_math`).  Only meaningful for non-entry
    /// modules; the entry module's exports are emitted at the top level.
    pub var: String,
}

/// Collect all transitive dependencies of `entry` in topological order
/// (each dependency appears before the modules that depend on it).
/// The entry module is always the last element.
pub fn collect(entry: &Path) -> Result<Vec<BundleModule>, String> {
    let canonical = entry
        .canonicalize()
        .map_err(|e| format!("{}: {}", entry.display(), e))?;
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut order: Vec<BundleModule> = Vec::new();
    let mut stems: HashMap<String, usize> = HashMap::new();
    collect_inner(&canonical, &mut visited, &mut order, &mut stems)?;
    Ok(order)
}

/// Like [`collect`] but accepts a pre-resolved canonical path (including
/// synthetic stdlib keys like `<lume:math>`).  Does not call `canonicalize()`.
pub fn collect_dep(canonical: &Path) -> Result<Vec<BundleModule>, String> {
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut order: Vec<BundleModule> = Vec::new();
    let mut stems: HashMap<String, usize> = HashMap::new();
    collect_inner(canonical, &mut visited, &mut order, &mut stems)?;
    Ok(order)
}

/// Build a Lua/JS-safe variable name from the module's file stem, deduplicating
/// with a numeric suffix if the same stem appears more than once.
fn make_var(canonical: &Path, stems: &mut HashMap<String, usize>) -> String {
    let stem: String = canonical
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("mod")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let idx = stems.entry(stem.clone()).or_insert(0);
    let var = if *idx == 0 {
        format!("_mod_{}", stem)
    } else {
        format!("_mod_{}_{}", stem, idx)
    };
    *idx += 1;
    var
}

fn collect_inner(
    canonical: &Path,
    visited: &mut HashSet<PathBuf>,
    order: &mut Vec<BundleModule>,
    stems: &mut HashMap<String, usize>,
) -> Result<(), String> {
    // Deduplicate: if already visited, nothing to do.
    if !visited.insert(canonical.to_owned()) {
        return Ok(());
    }

    // Read source - either from the embedded stdlib or the filesystem.
    let src_owned: String;
    let src: &str = if let Some(s) = stdlib_source_for_key(canonical) {
        s
    } else {
        src_owned = std::fs::read_to_string(canonical)
            .map_err(|e| format!("{}: {}", canonical.display(), e))?;
        &src_owned
    };

    let program = Loader::parse(src)?;

    // Auto-include the prelude unless the module opts out via `-- lume no_prelude`.
    if !program.pragmas.no_prelude {
        collect_inner(&prelude_path(), visited, order, stems)?;
    }

    // Recurse into dependencies first (post-order).
    // For stdlib modules the synthetic key is used as the base; they can't
    // have relative imports so resolve_path is never reached for them.
    let base = canonical.parent().unwrap_or(Path::new("."));
    for use_decl in &program.uses {
        let dep = if let Some(embedded) = stdlib_source(&use_decl.path) {
            let _ = embedded;
            stdlib_path(&use_decl.path)
        } else {
            resolve_path(&use_decl.path, base)?
        };
        collect_inner(&dep, visited, order, stems)?;
    }

    let var = make_var(canonical, stems);
    order.push(BundleModule {
        canonical: canonical.to_owned(),
        program,
        var,
    });
    Ok(())
}

/// Look up embedded source by the synthetic key produced by `stdlib_path`.
fn stdlib_source_for_key(key: &Path) -> Option<&'static str> {
    let s = key.to_str()?;
    // Synthetic keys look like `<lume:list>`.
    if s.starts_with('<') && s.ends_with('>') {
        stdlib_source(&s[1..s.len() - 1])
    } else {
        None
    }
}
