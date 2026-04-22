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
                constraint_names: std::collections::HashMap::new(),
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
    pub fn lua_name(&self) -> String {
        format!("__{}", self.name)
    }
    pub fn js_name(&self) -> String {
        format!("__{}", self.name)
    }
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
    Builtin {
        name: "concat_text",
        ty: ty!(Text -> (Text -> Text)),
        lua: "function(a) return function(b) return a .. b end end",
        js: "(a) => (b) => a + b",
    },
    Builtin {
        name: "concat_list",
        ty: ty!(a. [a] -> ([a] -> [a])),
        lua: concat!(
            "function(a) return function(b)\n",
            "  local r = {}\n",
            "  for i = 1, #a do r[#r+1] = a[i] end\n",
            "  for i = 1, #b do r[#r+1] = b[i] end\n",
            "  return r\n",
            "end end",
        ),
        js: "(a) => (b) => [...a, ...b]",
    },
    Builtin {
        name: "float_add",
        ty: ty!(Num -> Num -> Num),
        lua: "function(a) return function(b) return a + b end end",
        js: "(a) => (b) => a + b",
    },
    Builtin {
        name: "float_sub",
        ty: ty!(Num -> Num -> Num),
        lua: "function(a) return function(b) return a - b end end",
        js: "(a) => (b) => a - b",
    },
    Builtin {
        name: "float_mul",
        ty: ty!(Num -> Num -> Num),
        lua: "function(a) return function(b) return a * b end end",
        js: "(a) => (b) => a * b",
    },
    Builtin {
        name: "float_div",
        ty: ty!(Num -> Num -> Num),
        lua: "function(a) return function(b) return a / b end end",
        js: "(a) => (b) => a / b",
    },
];

/// Map-primitive builtins — only injected into the `lume:map` stdlib module.
/// User code never sees these names directly; they import via `use map = "lume:map"`.
pub static MAP_BUILTINS: &[Builtin] = &[
    Builtin {
        name: "map_empty",
        ty: ty!(k v. Map[k, v]),
        lua: "{}",
        js: "{}",
    },
    Builtin {
        name: "map_insert",
        ty: ty!(k v. k -> v -> (Map[k, v]) -> (Map[k, v])),
        lua: concat!(
            "function(k) return function(v) return function(m)\n",
            "  local r = {}\n",
            "  for key, val in pairs(m) do r[key] = val end\n",
            "  r[k] = v\n",
            "  return r\n",
            "end end end",
        ),
        js: "k => v => m => { const r = {...m}; r[k] = v; return r }",
    },
    Builtin {
        name: "map_get",
        ty: ty!(k v. k -> (Map[k, v]) -> Maybe[v]),
        lua: concat!(
            "function(k) return function(m)\n",
            "  local v = m[k]\n",
            "  if v ~= nil then return {_tag=\"Some\", _0=v}\n",
            "  else return {_tag=\"None\"} end\n",
            "end end",
        ),
        js: "k => m => k in m ? {$tag: \"Some\", _0: m[k]} : {$tag: \"None\"}",
    },
    Builtin {
        name: "map_delete",
        ty: ty!(k v. k -> (Map[k, v]) -> (Map[k, v])),
        lua: concat!(
            "function(k) return function(m)\n",
            "  local r = {}\n",
            "  for key, val in pairs(m) do if key ~= k then r[key] = val end end\n",
            "  return r\n",
            "end end",
        ),
        js: "k => m => { const r = {...m}; delete r[k]; return r }",
    },
    Builtin {
        name: "map_member",
        ty: ty!(k v. k -> (Map[k, v]) -> Bool),
        lua: "function(k) return function(m) return m[k] ~= nil end end",
        js: "k => m => k in m",
    },
    Builtin {
        name: "map_size",
        ty: ty!(k v. (Map[k, v]) -> Num),
        lua: concat!(
            "function(m)\n",
            "  local n = 0\n",
            "  for _ in pairs(m) do n = n + 1 end\n",
            "  return n\n",
            "end",
        ),
        js: "m => Object.keys(m).length",
    },
    Builtin {
        name: "map_keys",
        ty: ty!(k v. (Map[k, v]) -> [k]),
        lua: concat!(
            "function(m)\n",
            "  local r = {}\n",
            "  for key in pairs(m) do r[#r+1] = key end\n",
            "  return r\n",
            "end",
        ),
        js: "m => Object.keys(m)",
    },
    Builtin {
        name: "map_values",
        ty: ty!(k v. (Map[k, v]) -> [v]),
        lua: concat!(
            "function(m)\n",
            "  local r = {}\n",
            "  for _, val in pairs(m) do r[#r+1] = val end\n",
            "  return r\n",
            "end",
        ),
        js: "m => Object.values(m)",
    },
    Builtin {
        name: "map_to_list",
        ty: ty!(k v t. (Map[k, v]) -> [t]),
        lua: concat!(
            "function(m)\n",
            "  local r = {}\n",
            "  for key, val in pairs(m) do r[#r+1] = {key=key, val=val} end\n",
            "  return r\n",
            "end",
        ),
        js: "m => Object.entries(m).map(([k, v]) => ({key: k, val: v}))",
    },
    Builtin {
        name: "map_from_list",
        ty: ty!(k v t. [t] -> (Map[k, v])),
        lua: concat!(
            "function(xs)\n",
            "  local r = {}\n",
            "  for _, p in ipairs(xs) do r[p.key] = p.val end\n",
            "  return r\n",
            "end",
        ),
        js: "xs => Object.fromEntries(xs.map(p => [p.key, p.val]))",
    },
    Builtin {
        name: "map_union",
        ty: ty!(k v. (Map[k, v]) -> (Map[k, v]) -> (Map[k, v])),
        lua: concat!(
            "function(m1) return function(m2)\n",
            "  local r = {}\n",
            "  for k, v in pairs(m2) do r[k] = v end\n",
            "  for k, v in pairs(m1) do r[k] = v end\n",
            "  return r\n",
            "end end",
        ),
        js: "m1 => m2 => ({...m2, ...m1})",
    },
    Builtin {
        name: "map_map",
        ty: ty!(k v c. (v -> c) -> (Map[k, v]) -> (Map[k, c])),
        lua: concat!(
            "function(f) return function(m)\n",
            "  local r = {}\n",
            "  for k, v in pairs(m) do r[k] = f(v) end\n",
            "  return r\n",
            "end end",
        ),
        js: "f => m => Object.fromEntries(Object.entries(m).map(([k, v]) => [k, f(v)]))",
    },
    Builtin {
        name: "map_fold",
        ty: ty!(k v a. a -> (a -> k -> v -> a) -> (Map[k, v]) -> a),
        lua: concat!(
            "function(acc) return function(f) return function(m)\n",
            "  local a = acc\n",
            "  for k, v in pairs(m) do a = f(a)(k)(v) end\n",
            "  return a\n",
            "end end end",
        ),
        js: "acc => f => m => { let a = acc; for (const [k, v] of Object.entries(m)) a = f(a)(k)(v); return a }",
    },
];

/// Built-in type constructor arities used to seed `build_arity_env`.
pub const BUILTIN_TYPE_ARITIES: &[(&str, usize)] = &[
    ("Num", 0),
    ("Text", 0),
    ("Bool", 0),
    ("List", 1),
    ("Maybe", 1),
    ("Result", 2),
    ("Map", 2),
];

pub fn populate_env(s: &mut Subst, env: &mut TypeEnv) {
    for b in BUILTINS {
        env.insert(b.name.into(), (b.ty)(s));
    }
}

pub fn populate_map_env(s: &mut Subst, env: &mut TypeEnv) {
    for b in MAP_BUILTINS {
        env.insert(b.name.into(), (b.ty)(s));
    }
}
