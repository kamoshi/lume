use std::path::PathBuf;

use lume::{bundle::BundleModule, codegen, loader::Loader, types};
use wasm_bindgen::prelude::*;

fn make_bundle(src: &str) -> Result<Vec<BundleModule>, String> {
    let program = Loader::parse(src)?;
    Ok(vec![BundleModule {
        canonical: PathBuf::from("main.lume"),
        var: "_mod_main".to_string(),
        program,
    }])
}

/// Parse Lume source. Returns `"ok"` on success or an error string.
#[wasm_bindgen]
pub fn parse(src: &str) -> Result<JsValue, JsValue> {
    Loader::parse(src)
        .map(|_| JsValue::from_str("ok"))
        .map_err(|e| JsValue::from_str(&e))
}

/// Parse and type-check Lume source. Returns the inferred export type string on
/// success, or an error string.
#[wasm_bindgen]
pub fn typecheck(src: &str) -> Result<JsValue, JsValue> {
    let program = Loader::parse(src).map_err(|e| JsValue::from_str(&e))?;
    types::infer::check_program(&program, None)
        .map(|ty| JsValue::from_str(&ty.to_string()))
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Transpile Lume source to JavaScript. Type-checks first; returns generated
/// JS on success or an error string.
#[wasm_bindgen]
pub fn to_js(src: &str) -> Result<JsValue, JsValue> {
    let bundle = make_bundle(src).map_err(|e| JsValue::from_str(&e))?;
    types::infer::check_program(&bundle[0].program, None)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(JsValue::from_str(&codegen::js::emit(&bundle)))
}

/// Transpile Lume source to Lua. Type-checks first; returns generated Lua on
/// success or an error string.
#[wasm_bindgen]
pub fn to_lua(src: &str) -> Result<JsValue, JsValue> {
    let bundle = make_bundle(src).map_err(|e| JsValue::from_str(&e))?;
    types::infer::check_program(&bundle[0].program, None)
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    Ok(JsValue::from_str(&codegen::lua::emit(&bundle)))
}
