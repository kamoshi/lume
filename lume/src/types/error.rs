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
