//! Trait-system desugaring pass.
//!
//! Transforms a parsed `Program` that may contain `TraitDef`, `ImplDef`, and
//! `TraitCall` nodes into a plain `Program` that only uses core AST constructs,
//! so that the code-generation backends need no knowledge of the trait system.
//!
//! The transformation is:
//!
//! * `trait Show a { show: a -> Text }` → dropped (only used for type checking)
//! * `use Show in Num { show = expr }` → `let __show_Num = { show = expr }`
//! * `Show.show` (when `a ~ Num`, local impl) → `__show_Num.show`
//! * `Show.show` (when `a ~ Num`, impl in module `_mod_foo`) → `_mod_foo.__show_Num.show`
//!
//! The `node_types` map (from `types::infer::elaborate`) provides the concrete
//! inferred type at every expression node, which is what lets us pick the right
//! impl dictionary for each `TraitCall`.

use std::collections::HashMap;

use crate::ast::*;
use crate::error::Span;
use crate::types::Ty;
use crate::types::infer::VariantInfo;

// ── Public types ──────────────────────────────────────────────────────────────

/// One entry in the global impl table built from all bundle modules.
///
/// `module_var` is `None` for impls that live in the module currently being
/// desugared (dict accessed by bare name), or `Some("_mod_foo")` for impls
/// that live in a dependency (dict accessed as `_mod_foo.__trait_Type`).
pub struct ImplEntry {
    pub module_var: Option<String>,
    pub dict_ident: String,
}

/// Global trait/impl context built once from the full bundle and threaded into
/// every per-module `desugar` call.
pub struct GlobalCtx {
    /// trait_name → TraitDef (collected from all modules)
    pub traits: HashMap<String, TraitDef>,
    /// (trait_name, type_name) → ImplEntry
    pub impls: HashMap<(String, String), ImplEntry>,
    /// constructor_name → VariantInfo (all payload variants from all modules)
    pub variants: HashMap<String, VariantInfo>,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Desugar all trait constructs in `program`.
///
/// * `node_types` — concrete inferred types from `elaborate`, used to resolve
///   which impl dict each `TraitCall` should use.
/// * `global` — trait/impl index built from the whole bundle so cross-module
///   `TraitCall`s can be resolved to `_mod_dep.__trait_Type.method`.
pub fn desugar(
    program: Program,
    node_types: &HashMap<NodeId, Ty>,
    global: &GlobalCtx,
) -> Program {
    let cx = Cx { node_types, global };

    // Collect all impl dict names before consuming the program items.
    // These dicts must be included in the module export so cross-module
    // TraitCall desugaring can access `_mod_dep.__trait_Type.method`.
    let impl_dict_names: Vec<String> = program
        .items
        .iter()
        .filter_map(|item| {
            if let TopItem::ImplDef(id) = item {
                Some(dict_name(&id.trait_name, &id.type_name))
            } else {
                None
            }
        })
        .collect();

    let mut new_items = Vec::new();
    for item in program.items {
        match item {
            TopItem::TraitDef(_) => {}
            TopItem::ImplDef(id) => {
                new_items.push(impl_def_to_binding(id));
            }
            TopItem::Binding(b) => {
                new_items.push(TopItem::Binding(Binding {
                    pattern: b.pattern,
                    ty: b.ty,
                    value: cx.expr(b.value),
                }));
            }
            TopItem::BindingGroup(bs) => {
                let new_bs = bs
                    .into_iter()
                    .map(|b| Binding {
                        pattern: b.pattern,
                        ty: b.ty,
                        value: cx.expr(b.value),
                    })
                    .collect();
                new_items.push(TopItem::BindingGroup(new_bs));
            }
            TopItem::TypeDef(td) => new_items.push(TopItem::TypeDef(td)),
        }
    }

    // Augment the export expression to include all impl dicts so that
    // cross-module trait calls (`_mod_dep.__trait_Type.method`) resolve.
    let mut exports = cx.expr(program.exports);
    if !impl_dict_names.is_empty() {
        if let ExprKind::Record { ref mut fields, .. } = exports.kind {
            for dict in impl_dict_names {
                fields.push(RecordField {
                    name: dict.clone(),
                    name_span: Span::default(),
                    name_node_id: 0,
                    value: Some(Expr {
                        id: 0,
                        kind: ExprKind::Ident(dict),
                        span: Span::default(),
                    }),
                });
            }
        }
    }

    Program {
        uses: program.uses,
        items: new_items,
        exports,
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

struct Cx<'a> {
    node_types: &'a HashMap<NodeId, Ty>,
    global: &'a GlobalCtx,
}

impl<'a> Cx<'a> {
    fn expr(&self, e: Expr) -> Expr {
        let id = e.id;
        let span = e.span.clone();
        match e.kind {
            ExprKind::TraitCall { trait_name, method_name } => {
                if let Some(desugared) =
                    self.resolve_trait_call(id, &trait_name, &method_name, span.clone())
                {
                    desugared
                } else {
                    Expr {
                        id,
                        kind: ExprKind::TraitCall { trait_name, method_name },
                        span,
                    }
                }
            }

            ExprKind::List(items) => Expr {
                id,
                kind: ExprKind::List(items.into_iter().map(|e| self.expr(e)).collect()),
                span,
            },
            ExprKind::Record { base, fields, spread } => {
                let base = base.map(|b| Box::new(self.expr(*b)));
                let fields = fields
                    .into_iter()
                    .map(|f| RecordField {
                        value: f.value.map(|v| self.expr(v)),
                        ..f
                    })
                    .collect();
                Expr { id, kind: ExprKind::Record { base, fields, spread }, span }
            }
            ExprKind::FieldAccess { record, field } => Expr {
                id,
                kind: ExprKind::FieldAccess {
                    record: Box::new(self.expr(*record)),
                    field,
                },
                span,
            },
            ExprKind::Variant { name, payload: None } => {
                // Bare constructor reference (no payload in the AST).
                // If this variant has a payload, desugar to a constructor lambda:
                //   fun __p -> Name { f1: __p.f1, f2: __p.f2, ... }
                if let Some(info) = self.global.variants.get(&name) {
                    if let Some(fields) = &info.payload_fields {
                        let param_name = "__p".to_string();
                        let payload_fields: Vec<RecordField> = fields
                            .iter()
                            .map(|(fname, _)| RecordField {
                                name: fname.clone(),
                                name_span: span.clone(),
                                name_node_id: 0,
                                value: Some(Expr {
                                    id: 0,
                                    kind: ExprKind::FieldAccess {
                                        record: Box::new(Expr {
                                            id: 0,
                                            kind: ExprKind::Ident(param_name.clone()),
                                            span: span.clone(),
                                        }),
                                        field: fname.clone(),
                                    },
                                    span: span.clone(),
                                }),
                            })
                            .collect();
                        return Expr {
                            id,
                            kind: ExprKind::Lambda {
                                param: Pattern::Ident(param_name, span.clone(), 0),
                                body: Box::new(Expr {
                                    id: 0,
                                    kind: ExprKind::Variant {
                                        name,
                                        payload: Some(Box::new(Expr {
                                            id: 0,
                                            kind: ExprKind::Record {
                                                base: None,
                                                fields: payload_fields,
                                                spread: false,
                                            },
                                            span: span.clone(),
                                        })),
                                    },
                                    span: span.clone(),
                                }),
                            },
                            span,
                        };
                    }
                }
                Expr { id, kind: ExprKind::Variant { name, payload: None }, span }
            }
            ExprKind::Variant { name, payload: Some(payload) } => Expr {
                id,
                kind: ExprKind::Variant {
                    name,
                    payload: Some(Box::new(self.expr(*payload))),
                },
                span,
            },
            ExprKind::Lambda { param, body } => Expr {
                id,
                kind: ExprKind::Lambda {
                    param,
                    body: Box::new(self.expr(*body)),
                },
                span,
            },
            ExprKind::Apply { func, arg } => Expr {
                id,
                kind: ExprKind::Apply {
                    func: Box::new(self.expr(*func)),
                    arg: Box::new(self.expr(*arg)),
                },
                span,
            },
            ExprKind::Binary { op, left, right } => Expr {
                id,
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(self.expr(*left)),
                    right: Box::new(self.expr(*right)),
                },
                span,
            },
            ExprKind::Unary { op, operand } => Expr {
                id,
                kind: ExprKind::Unary {
                    op,
                    operand: Box::new(self.expr(*operand)),
                },
                span,
            },
            ExprKind::If { cond, then_branch, else_branch } => Expr {
                id,
                kind: ExprKind::If {
                    cond: Box::new(self.expr(*cond)),
                    then_branch: Box::new(self.expr(*then_branch)),
                    else_branch: Box::new(self.expr(*else_branch)),
                },
                span,
            },
            ExprKind::Match(arms) => Expr {
                id,
                kind: ExprKind::Match(
                    arms.into_iter()
                        .map(|arm| MatchArm {
                            pattern: arm.pattern,
                            guard: arm.guard.map(|g| self.expr(g)),
                            body: self.expr(arm.body),
                        })
                        .collect(),
                ),
                span,
            },
            ExprKind::LetIn { pattern, value, body } => Expr {
                id,
                kind: ExprKind::LetIn {
                    pattern,
                    value: Box::new(self.expr(*value)),
                    body: Box::new(self.expr(*body)),
                },
                span,
            },
            other => Expr { id, kind: other, span },
        }
    }

    fn resolve_trait_call(
        &self,
        node_id: NodeId,
        trait_name: &str,
        method_name: &str,
        span: Span,
    ) -> Option<Expr> {
        let call_ty = self.node_types.get(&node_id)?;
        let type_name = self.extract_type_param(trait_name, method_name, call_ty)?;
        let entry = self.global.impls.get(&(trait_name.to_string(), type_name))?;

        // Build the dict reference: either bare `__trait_Type` or `_mod_foo.__trait_Type`
        let dict_expr = match &entry.module_var {
            None => Expr {
                id: 0,
                kind: ExprKind::Ident(entry.dict_ident.clone()),
                span: span.clone(),
            },
            Some(mod_var) => Expr {
                id: 0,
                kind: ExprKind::FieldAccess {
                    record: Box::new(Expr {
                        id: 0,
                        kind: ExprKind::Ident(mod_var.clone()),
                        span: span.clone(),
                    }),
                    field: entry.dict_ident.clone(),
                },
                span: span.clone(),
            },
        };

        Some(Expr {
            id: node_id,
            kind: ExprKind::FieldAccess {
                record: Box::new(dict_expr),
                field: method_name.to_string(),
            },
            span,
        })
    }

    fn extract_type_param(
        &self,
        trait_name: &str,
        method_name: &str,
        concrete: &Ty,
    ) -> Option<String> {
        let trait_def = self.global.traits.get(trait_name)?;
        let method_decl = trait_def.methods.iter().find(|m| m.name == method_name)?;
        find_type_param_in_ast_type(&method_decl.ty, &trait_def.type_param, concrete)
    }
}

fn find_type_param_in_ast_type(ast_ty: &Type, param_name: &str, concrete: &Ty) -> Option<String> {
    match ast_ty {
        Type::Var(v) if v == param_name => ty_head_name(concrete),
        Type::Func { param, ret } => {
            if let Ty::Func(cp, cr) = concrete {
                find_type_param_in_ast_type(param, param_name, cp)
                    .or_else(|| find_type_param_in_ast_type(ret, param_name, cr))
            } else {
                None
            }
        }
        Type::Named { args, .. } => {
            let concrete_args = match concrete {
                Ty::Con(_, args) => args.as_slice(),
                Ty::List(inner) => std::slice::from_ref(inner.as_ref()),
                _ => return None,
            };
            args.iter()
                .zip(concrete_args.iter())
                .find_map(|(a, c)| find_type_param_in_ast_type(a, param_name, c))
        }
        Type::Record(rt) => {
            if let Ty::Record(row) = concrete {
                rt.fields.iter().find_map(|f| {
                    row.fields
                        .iter()
                        .find(|(name, _)| name == &f.name)
                        .and_then(|(_, ty)| find_type_param_in_ast_type(&f.ty, param_name, ty))
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

fn ty_head_name(ty: &Ty) -> Option<String> {
    match ty {
        Ty::Num => Some("Num".to_string()),
        Ty::Text => Some("Text".to_string()),
        Ty::Bool => Some("Bool".to_string()),
        Ty::Con(name, _) => Some(name.clone()),
        Ty::List(_) => Some("List".to_string()),
        _ => None,
    }
}

/// Canonical dict binding name: `Show` + `Num` → `__show_Num`
pub fn dict_name(trait_name: &str, type_name: &str) -> String {
    format!("__{}_{}", trait_name.to_ascii_lowercase(), type_name)
}

fn impl_def_to_binding(id: ImplDef) -> TopItem {
    let dict = dict_name(&id.trait_name, &id.type_name);
    let fields: Vec<RecordField> = id
        .methods
        .into_iter()
        .map(|b| {
            let name = match &b.pattern {
                Pattern::Ident(n, _, _) => n.clone(),
                _ => panic!("impl method patterns must be simple identifiers"),
            };
            RecordField {
                name,
                name_span: Span::default(),
                name_node_id: 0,
                value: Some(b.value),
            }
        })
        .collect();
    TopItem::Binding(Binding {
        pattern: Pattern::Ident(dict, Span::default(), 0),
        ty: None,
        value: Expr {
            id: 0,
            kind: ExprKind::Record { base: None, fields, spread: false },
            span: Span::default(),
        },
    })
}
