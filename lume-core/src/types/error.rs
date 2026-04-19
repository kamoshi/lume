use crate::error::Span;
use crate::types::{Ty, TyVar};
use std::fmt;

#[derive(Debug, Clone)]
pub enum TypeError {
    UnboundVariable(String),
    UnboundVariant(String),
    Mismatch(Ty, Ty),
    OccursCheck(TyVar),
    /// Record unification failed because of this unexpected field.
    RowMismatch(String),
    FieldNotFound {
        field: String,
        record_ty: Ty,
    },
    ConcatNonConcatenable(Ty),
    GuardNotBool(Ty),
    ResultPipeNonResult(Ty),
    ImportError(String),
    /// Non-exhaustive match: missing variants listed.
    NonExhaustiveMatch(Vec<String>),
    /// Typed hole `_`: the inferred expected type is shown to the user.
    TypedHole(Ty),
    /// A trait method was used but no matching impl exists.
    MissingImpl {
        trait_name: String,
        type_name: String,
    },
    /// An impl block is missing required trait methods.
    IncompleteImpl {
        trait_name: String,
        type_name: String,
        missing: Vec<String>,
    },
    /// Duplicate impl for the same trait+type across modules.
    DuplicateImpl {
        trait_name: String,
        type_name: String,
    },
    /// An impl block has methods not declared in the trait.
    ExtraImplMethods {
        trait_name: String,
        type_name: String,
        extra: Vec<String>,
    },
    /// A trait impl targets an open record type (one with `..` / row variable).
    /// Only closed records may implement traits.
    ImplForOpenRecord {
        trait_name: String,
    },
    /// A trait method does not reference the trait's type parameter.
    TraitMethodMissingParam {
        trait_name: String,
        method_name: String,
        type_param: String,
    },
    /// An impl references a trait that has not been declared (or is declared
    /// after the impl — traits must be defined before use).
    UndeclaredTrait {
        trait_name: String,
    },
    /// A type constructor was applied to the wrong number of arguments.
    ArityMismatch {
        name: String,
        expected: usize,
        actual: usize,
    },
    /// A bare method name refers to methods in multiple traits; use qualified
    /// syntax (e.g. `Functor.fmap`) to disambiguate.
    AmbiguousTraitMethod {
        name: String,
        options: Vec<String>,
    },
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::UnboundVariable(n) => write!(f, "unbound variable '{}'", n),
            TypeError::UnboundVariant(n) => write!(f, "unbound variant '{}'", n),
            TypeError::Mismatch(t1, t2) => {
                write!(f, "type mismatch: expected `{}`, got `{}`", t1, t2)
            }
            TypeError::OccursCheck(v) => write!(
                f,
                "occurs check: ?{} appears in its own type (infinite type)",
                v
            ),
            TypeError::RowMismatch(field) => {
                write!(f, "record row mismatch: unexpected field '{}'", field)
            }
            TypeError::FieldNotFound { field, record_ty } => {
                write!(f, "field '{}' not found in `{}`", field, record_ty)
            }
            TypeError::ConcatNonConcatenable(ty) => write!(
                f,
                "cannot concatenate `{}` (only Text, List, or Record)",
                ty
            ),
            TypeError::GuardNotBool(ty) => write!(f, "match guard must be Bool, got `{}`", ty),
            TypeError::ResultPipeNonResult(ty) => {
                write!(f, "?> requires a Result type on the left, got `{}`", ty)
            }
            TypeError::ImportError(msg) => write!(f, "import error: {}", msg),
            TypeError::NonExhaustiveMatch(missing) => {
                write!(f, "non-exhaustive match, missing: {}", missing.join(", "))
            }
            TypeError::TypedHole(ty) => {
                write!(f, "typed hole: found type `{}`", ty)
            }
            TypeError::MissingImpl { trait_name, type_name } => {
                write!(f, "no impl of trait '{}' for type '{}'", trait_name, type_name)
            }
            TypeError::IncompleteImpl { trait_name, type_name, missing } => {
                write!(
                    f,
                    "impl {} for {}: missing method(s): {}",
                    trait_name, type_name, missing.join(", ")
                )
            }
            TypeError::DuplicateImpl { trait_name, type_name } => {
                write!(
                    f,
                    "duplicate impl of trait '{}' for type '{}'",
                    trait_name, type_name
                )
            }
            TypeError::ExtraImplMethods { trait_name, type_name, extra } => {
                write!(
                    f,
                    "impl {} for {}: unknown method(s) not in trait: {}",
                    trait_name, type_name, extra.join(", ")
                )
            }
            TypeError::ImplForOpenRecord { trait_name } => {
                write!(
                    f,
                    "cannot implement trait '{}' for an open record type — \
                     only closed records (without `..`) may implement traits, \
                     because an open row admits unknown fields that could \
                     violate the trait's contract",
                    trait_name
                )
            }
            TypeError::TraitMethodMissingParam { trait_name, method_name, type_param } => {
                write!(
                    f,
                    "trait '{}' method '{}' does not reference type parameter '{}' — \
                     the method signature must use '{}' so the compiler can connect it \
                     to concrete types in impls",
                    trait_name, method_name, type_param, type_param
                )
            }
            TypeError::UndeclaredTrait { trait_name } => {
                write!(
                    f,
                    "undeclared trait '{}' — trait must be defined before it is implemented",
                    trait_name
                )
            }
            TypeError::ArityMismatch { name, expected, actual } => {
                write!(
                    f,
                    "type '{}' expects {} type argument(s) but was given {}",
                    name, expected, actual
                )
            }
            TypeError::AmbiguousTraitMethod { name, options } => {
                write!(
                    f,
                    "ambiguous trait method '{}': could be {}; use qualified syntax to disambiguate",
                    name,
                    options.join(" or ")
                )
            }
        }
    }
}

/// A `TypeError` paired with the source location where it was raised.
#[derive(Debug, Clone)]
pub struct TypeErrorAt {
    pub error: TypeError,
    pub span: Span,
}

impl TypeErrorAt {
    pub fn new(error: TypeError, span: Span) -> Self {
        TypeErrorAt { error, span }
    }
}

impl fmt::Display for TypeErrorAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.span, self.error)
    }
}
