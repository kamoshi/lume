use std::path::PathBuf;

pub mod js;
pub mod lua;

/// A module bundled with its lowered IR, ready for code generation.
pub struct IrModule {
    pub canonical: PathBuf,
    pub module: crate::ir::Module,
    pub var: String,
}
