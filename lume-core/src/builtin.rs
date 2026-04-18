use crate::types::{infer::TypeEnv, Scheme, Subst, Ty};

// Builds a Ty value from type syntax.
macro_rules! type_def {
    // Base primitives
    (Num)  => { Ty::Num };
    (Bool) => { Ty::Bool };
    (Text) => { Ty::Text };

    // Parentheses
    (( $($inner:tt)+ )) => { type_def!($($inner)+) };

    // Function arrow (right-associative)
    ($lhs:tt -> $($rhs:tt)+) => {
        Ty::Func(Box::new(type_def!($lhs)), Box::new(type_def!($($rhs)+)))
    };

    // List sugar: [Num] instead of List Num
    ([$($inner:tt)+]) => { Ty::App(Box::new(Ty::Con("List".into())), Box::new(type_def!($($inner)+))) };

    // Generic constructors: Maybe[a], Result[a, e] → curried App chain
    ($con:ident [ $($arg:tt),* ]) => {
        type_def!(@app Ty::Con(stringify!($con).into()), $( $arg ),* )
    };

    // Internal: fold arguments into a left-associative App chain.
    (@app $acc:expr, $head:tt $( , $rest:tt )*) => {
        type_def!(@app Ty::App(Box::new($acc), Box::new(type_def!($head))) $( , $rest )* )
    };
    (@app $acc:expr $(,)?) => { $acc };

    // Variable fallback: a Rust variable holding a TyVar
    ($v:ident) => { Ty::Var($v) };
}

// Produces a fn(&mut Subst) -> Scheme that can be stored in a static.
//
//   ty!(a b. Text -> a)   — polymorphic: allocates fresh vars, builds Scheme
//   ty!(Text -> Num)      — monomorphic: no vars needed
macro_rules! ty {
    ($($v:ident)+ . $($body:tt)+) => {
        (|s: &mut Subst| {
            $(let $v = s.fresh_var();)*
            Scheme {
                vars: vec![$($v),*],
                row_vars: vec![],
                constraints: vec![],
                ty: type_def!($($body)+),
            }
        })
    };

    ($($body:tt)+) => {
        (|_: &mut Subst| Scheme::mono(type_def!($($body)+)))
    };
}

pub struct Builtin {
    pub name: &'static str,
    pub ty: fn(&mut Subst) -> Scheme,
    pub lua: &'static str,
    pub js: &'static str,
}

impl Builtin {
    pub fn lua_name(&self) -> String { format!("__{}", self.name) }
    pub fn js_name(&self)  -> String { format!("__{}", self.name) }
}

pub static BUILTINS: &[Builtin] = &[
    Builtin {
        name: "error",
        ty: ty!(a. Text -> a),
        lua: "function(msg) error(msg) end",
        js: "(msg) => { throw new Error(msg); }",
    },
    Builtin {
        name: "trim",
        ty: ty!(Text -> Text),
        lua: "function(s) return (s:gsub('^%s+', ''):gsub('%s+$', '')) end",
        js: "(s) => s.trim()",
    },
];

pub fn populate_env(s: &mut Subst, env: &mut TypeEnv) {
    for b in BUILTINS {
        env.insert(b.name.into(), (b.ty)(s));
    }
}
