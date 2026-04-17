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
    ([$($inner:tt)+]) => { Ty::List(Box::new(type_def!($($inner)+))) };

    // Generic constructors: Maybe[a], Result[a, e]
    ($con:ident [ $($arg:tt),* ]) => {
        Ty::Con(stringify!($con).into(), vec![ $( type_def!($arg) ),* ])
    };

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

pub struct BuiltinImpl {
    pub name: &'static str,
    pub body: &'static str,
}

pub struct Builtin {
    pub name: &'static str,
    pub ty: fn(&mut Subst) -> Scheme,
    pub lua: BuiltinImpl,
    pub js: BuiltinImpl,
}

pub static BUILTINS: &[Builtin] = &[Builtin {
    name: "error",
    ty: ty!(a. Text -> a),
    lua: BuiltinImpl {
        name: "__error",
        body: "local function __error(msg) error(msg) end\n\n",
    },
    js: BuiltinImpl {
        name: "__error",
        body: "function __error(msg) { throw new Error(msg); }\n\n",
    },
}];

pub fn populate_env(s: &mut Subst, env: &mut TypeEnv) {
    for b in BUILTINS {
        env.insert(b.name.into(), (b.ty)(s));
    }
}
