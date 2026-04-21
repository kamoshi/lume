//! Shared compilation pipeline: type-check → lower → optimise.
//!
//! Each front-end crate (`lume`, `lume-repl`, `lume-wasm`) calls
//! [`lower_bundle`] instead of duplicating the lower/optimise sequence.
//! Adding a new IR pass means editing only this file.

use std::collections::HashMap;

use crate::ast::TopItem;
use crate::bundle::BundleModule;
use crate::codegen::IrModule;
use crate::ir;
use crate::lower;
use crate::types;

/// Type-check, lower, and optimise every module in a bundle.
///
/// Returns `Err(message)` on the first type error encountered, where
/// `message` includes the canonical module path.
pub fn lower_bundle(
    b: &[BundleModule],
) -> Result<(Vec<IrModule>, types::infer::VariantEnv), String> {
    // ── 1. Build the global trait / impl / variant context ───────────────────
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

    // Register built-in variants (Maybe, Result) so bare constructor
    // references are correctly desugared by the lowerer.
    {
        let mut scratch = types::Subst::new();
        let (_, builtin_variants) = types::infer::builtin_env(&mut scratch);
        for (name, info) in builtin_variants.all() {
            global.variants.entry(name.clone()).or_insert_with(|| info.clone());
        }
    }

    // ── 2. Lower and optimise each module ────────────────────────────────────
    let mut ir_modules = Vec::new();

    for m in b.iter() {
        // Build a module-local view of the global context: impl dicts defined
        // in *this* module are accessed by bare name; others via module_var.
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
        let (node_types, type_env, resolved_trait_methods, resolved_op_types) =
            types::infer::elaborate_with_env(&m.program, module_path)
                .map(|(nt, env, _, rtm, rot)| (nt, env, rtm, rot))
                .map_err(|e| format!("{}: type error: {e}", m.canonical.display()))?;

        let ir_mod = lower::lower(
            m.program.clone(),
            &node_types,
            &type_env,
            &local_global,
            &resolved_trait_methods,
            &resolved_op_types,
        );

        // ── IR optimisation passes (add new passes here) ──────────────────
        let ir_mod = ir::dict_hoist::hoist_dict_applications(ir_mod);
        let ir_mod = ir::eta::eta_reduce(ir_mod);

        ir_modules.push(IrModule {
            canonical: m.canonical.clone(),
            module: ir_mod,
            var: m.var.clone(),
        });
    }

    // ── 3. Build the variant environment for codegen ─────────────────────────
    let mut variant_env = types::infer::VariantEnv::default();
    for (name, info) in global.variants {
        variant_env.insert(name, info);
    }

    Ok((ir_modules, variant_env))
}
