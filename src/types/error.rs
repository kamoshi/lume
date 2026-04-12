use std::fmt;
use crate::types::{Ty, TyVar};

#[derive(Debug, Clone)]
pub enum TypeError {
    UnboundVariable(String),
    UnboundVariant(String),
    Mismatch(Ty, Ty),
    OccursCheck(TyVar),
    /// Record unification failed because of this unexpected field.
    RowMismatch(String),
    FieldNotFound { field: String, record_ty: Ty },
    NotAFunction(Ty),
    ConcatNonConcatenable(Ty),
    GuardNotBool(Ty),
    ResultPipeNonResult(Ty),
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::UnboundVariable(n) =>
                write!(f, "unbound variable '{}'", n),
            TypeError::UnboundVariant(n) =>
                write!(f, "unbound variant '{}'", n),
            TypeError::Mismatch(t1, t2) =>
                write!(f, "type mismatch: expected `{}`, got `{}`", t1, t2),
            TypeError::OccursCheck(v) =>
                write!(f, "occurs check: ?{} appears in its own type (infinite type)", v),
            TypeError::RowMismatch(field) =>
                write!(f, "record row mismatch: unexpected field '{}'", field),
            TypeError::FieldNotFound { field, record_ty } =>
                write!(f, "field '{}' not found in `{}`", field, record_ty),
            TypeError::NotAFunction(ty) =>
                write!(f, "not a function: `{}`", ty),
            TypeError::ConcatNonConcatenable(ty) =>
                write!(f, "cannot concatenate `{}` (only Text, List, or Record)", ty),
            TypeError::GuardNotBool(ty) =>
                write!(f, "match guard must be Bool, got `{}`", ty),
            TypeError::ResultPipeNonResult(ty) =>
                write!(f, "?> requires a Result type on the left, got `{}`", ty),
        }
    }
}
