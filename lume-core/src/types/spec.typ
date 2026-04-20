#set page(paper: "a4", margin: 1cm)

= Formal Type System: Lume Logic

== 1. Syntax

*Types* ($tau$) and *Rows* ($r$):
$
  tau & ::= "Num" | "Text" | "Bool" | alpha | tau_1 -> tau_2 | C | tau_1 tau_2 | { r } \
    r & ::= emptyset | ell : tau , r | rho
$

Type constructors ($C$) are nullary names like `"List"`, `"Maybe"`, `"Result"`,
or user-defined ADTs. Multi-argument constructors are applied via curried $tau_1
tau_2$: e.g. `Result Num Text` is $"Result" "Num" "Text"$ (left-associative
application).

== 2. Core Operations

- *Instantiation* ($"inst"(sigma)$): Matches `Checker::instantiate`.
  Replaces quantified type and row variables with fresh ones, and propagates the scheme's
  constraints (renamed with the fresh variables) into the current constraint map:
  $
    "inst"(forall overline(alpha) overline(rho) . overline(C) => tau) = ([overline(beta) \/ overline(alpha), overline(rho') \/ overline(rho)] tau, quad [overline(beta) \/ overline(alpha)] overline(C))
  $

- *Generalization* ($"Gen"(Gamma, tau)$): Matches `Checker::generalise`.
  Collects free type and row variables not bound in $Gamma$, gathers all constraints from
  the constraint map that mention those variables, deduplicates, and quantifies:
  $
    "Gen"(Gamma, tau) = forall overline(alpha) overline(rho) . overline(C) => tau quad "where" quad overline(alpha), overline(rho) = "ftv"(tau) without "ftv"(Gamma) quad "and" quad overline(C) = "constraints on" overline(alpha)
  $

== 3. Synthesis Rules ($Gamma tack.r e => tau$)

=== 3.1 Literals

Literal forms are axioms — they require no premises:

$
  "[Num]" quad Gamma tack.r n => "Num" quad quad quad "[Text]" quad Gamma tack.r s => "Text" quad quad quad "[Bool]" quad Gamma tack.r b => "Bool"
$

=== 3.2 Typed Hole

A hole `_` in expression position allocates a fresh unification variable ($alpha$ fresh) whose resolved type is reported as a diagnostic:

$ "[Hole]" quad Gamma tack.r_=> alpha $

=== 3.3 List

A list literal may contain plain elements and *spread* entries (`..e`).
Plain elements must unify with a common element type $alpha$; each spread
must unify with $"List" alpha$:

$
  "[List]" quad (forall i in "Elem" : Gamma tack.r e_i => tau_i quad tau_i tilde.equiv alpha quad quad forall j in "Spread" : Gamma tack.r s_j => "List" alpha) / (Gamma tack.r [ dots.h ] => "List" alpha)
$

An empty list (no elements, no spreads) leaves the element type polymorphic.

=== 3.4 Variable

$ "[Var]" quad (x : sigma in Gamma quad tau = "inst"(sigma)) / (Gamma tack.r x => tau) $

If $x$ is not in $Gamma$ but exactly one trait defines a method named $x$, it is resolved
automatically as a trait call with a fresh constraint variable (see *TraitCall* below).
If multiple traits define $x$, the reference is rejected as ambiguous.

=== 3.5 Lambda

A fresh type variable $alpha$ is allocated for the parameter; the body is inferred in the
extended environment:

$
  "[Lam]" quad ("fresh" alpha quad p tack.r.double alpha => Delta quad Gamma , Delta tack.r e => tau) / (Gamma tack.r lambda p . e => alpha -> tau)
$

=== 3.6 Application

$
  "[App]" quad (Gamma tack.r e_1 => tau_1 quad Gamma tack.r e_2 => tau_2 quad tau_1 tilde.equiv tau_2 -> alpha) / (Gamma tack.r e_1 e_2 => alpha)
$

=== 3.7 If–Then–Else

Both branches must have the same type:

$
  "[If]" quad (Gamma tack.r e_c => "Bool" quad Gamma tack.r e_t => tau quad Gamma tack.r e_e => tau) / (Gamma tack.r "if " e_c " then " e_t " else " e_e => tau)
$

=== 3.8 Unary Operators

$ "[Neg]" quad (Gamma tack.r e => "Num") / (Gamma tack.r - e => "Num") $

$ "[Not]" quad (Gamma tack.r e => "Bool") / (Gamma tack.r "not " e => "Bool") $

=== 3.9 Binary Operators

*Arithmetic* ($"op" in {+, -, times, div}$):

$ "[Arith]" quad (Gamma tack.r e_1 => "Num" quad Gamma tack.r e_2 => "Num") / (Gamma tack.r e_1 "op" e_2 => "Num") $

*Numeric comparison* ($"op" in {<, >, lt.eq, gt.eq}$):

$ "[Cmp]" quad (Gamma tack.r e_1 => "Num" quad Gamma tack.r e_2 => "Num") / (Gamma tack.r e_1 "op" e_2 => "Bool") $

*Equality* ($"op" in {=, eq.not}$) — operands must be the same type:

$ "[Eq]" quad (Gamma tack.r e_1 => tau quad Gamma tack.r e_2 => tau) / (Gamma tack.r e_1 "op" e_2 => "Bool") $

*Boolean* ($"op" in {and, or}$):

$ "[BoolOp]" quad (Gamma tack.r e_1 => "Bool" quad Gamma tack.r e_2 => "Bool") / (Gamma tack.r e_1 "op" e_2 => "Bool") $

*Concatenation* ($"++"$) and *custom operators* — resolved as a regular function call via
trait or local binding. The operator `e_1 "op" e_2` desugars to applying the binary function
named `op` (resolved via env lookup or trait shorthand) to both operands:

$
  "[OpCall]" quad ("op" : sigma in Gamma union "traits" quad tau_"op" = "inst"(sigma) quad Gamma tack.r e_1 => tau_1 quad tau_"op" tilde.equiv tau_1 -> (tau_2 -> tau_r) quad Gamma tack.r e_2 arrow.l tau_2) / (Gamma tack.r e_1 "op" e_2 => tau_r)
$

The standard library defines `trait Concat a { let (++) : a -> a -> a }` with implementations
for `Text` and `List a`. Files using `-- lume internal no_prelude` may instead define
`let (++) = concat_text` to make the operator available without the trait.

*Pipe* ($|>$) — equivalent to reversed application:

$
  "[Pipe]" quad (Gamma tack.r e_1 => tau_1 quad Gamma tack.r e_2 => tau_1 -> tau_2) / (Gamma tack.r e_1 "|>" e_2 => tau_2)
$

*Result pipe* ($?>$) — maps over the $"Ok"$ branch, threading the error type:

$
  "[ResultPipe]" quad (Gamma tack.r e_1 => "Result" alpha epsilon quad Gamma tack.r e_2 => alpha -> "Result" beta epsilon) / (Gamma tack.r e_1 "?>" e_2 => "Result" beta epsilon)
$

=== 3.10 Match (arm-list form)

A bare arm list $| p_1 -> e_1 | dots | p_n -> e_n$ synthesizes as a function.
Fresh variables $tau_"in"$ and $tau_"out"$ are shared across all arms.
Each arm's guard (if present) must have type $"Bool"$:

$
  "[Match]" quad (forall i : p_i tack.r.double tau_"in" => Delta_i quad (Gamma , Delta_i tack.r g_i => "Bool") quad Gamma , Delta_i tack.r e_i => tau_"out") / (Gamma tack.r (| p_1 -> e_1 | dots | p_n -> e_n) => tau_"in" -> tau_"out")
$

=== 3.11 Match Expression

A $"match"$ expression has an explicit scrutinee; the result type is the common body type.
Exhaustiveness is checked after all arms are typed:

$
  "[MatchExpr]" quad (Gamma tack.r e_s => tau_s quad forall i : p_i tack.r.double tau_s => Delta_i quad (Gamma , Delta_i tack.r g_i => "Bool") quad Gamma , Delta_i tack.r e_i => tau_"out" quad "exhaustive"(tau_s, overline(p))) / (Gamma tack.r "match " e_s " | " overline(p -> e) => tau_"out")
$

*Note on exhaustiveness*: the check only applies to known sum types (ADTs registered in
`variant_env`). Primitives (`Num`, `Text`, `Bool`) and records are not exhaustiveness-checked.
A guarded arm (`| p if g -> e`) does *not* count as covering the pattern's constructor:
since the guard may fail at runtime, only *non-guarded* arms contribute to exhaustiveness.
A wildcard (`_`) or bare ident pattern counts as a catch-all only when non-guarded.

=== 3.12 Trait Call

An explicit qualified call $T . m$ looks up the method type and records a constraint on the
trait's type parameter:

$
  "[TraitCall]" quad (T . m : tau in "trait\_env" quad "fresh" alpha quad C = (T , alpha)) / (Gamma tack.r T . m => [alpha \/ T."self"] tau quad "with constraint " C)
$

If the trait is not in `trait_env` (e.g. cross-module and not yet imported), the expression
falls back to a fresh type variable — errors will be caught during constraint checking.

=== 3.13 Record

Records contain an interleaved sequence of named *fields* (`ell : e`) and *spread*
expressions (`..e`). Entries are processed strictly left-to-right; later entries shadow
earlier ones with the same field name. Duplicate explicit field names are rejected.

Field shorthand: `{ age }` is sugar for `{ age: age }` — the field name is looked up in the
environment and its instantiated type is used:

$
  "[Record]" quad (forall k : "entry"_k "typed left-to-right" quad "fields"(overline("entry")) = overline(ell : tau) quad "spreads"(overline("entry")) => overline(r)) / (Gamma tack.r { "entry"_1 , dots , "entry"_n } => { overline("guaranteed") | rho })
$

The merge is left-to-right: in `{ ..a, x: 1, ..b }`, fields from `b` shadow `x: 1` if `b`
contains an `x` field. The result row tail $rho$ is determined as follows:
- If no spread has an open tail: closed.
- If exactly one spread has an open tail $rho_s$: the result reuses $rho_s$ directly,
  preserving the connection so that fields discovered through later unification propagate.
- If multiple spreads have open tails: they are unified into a single row variable,
  constraining the spreads to share the same residual fields.

*Soundness invariant (field guarantee)*: only fields that are _guaranteed_ to have their
stated type appear in the result's explicit field set. A field is guaranteed when it was
last set (explicitly or via a closed spread) _after_ the last open spread in entry order.
Fields set before an open spread may be shadowed at runtime (right always shadows left) and
are therefore excluded from the explicit row fields — their types are carried only by the
open tail variable.

Example: `{ x: 1, ..r }` with `r : { ..ρ }` produces `{ ..ρ }` (not `{ x: Num | ρ }`),
because `r` is to the right of `x` and could shadow it.  Conversely `{ ..r, x: 1 }` produces
`{ x: Num | ρ }` because `x: 1` is guaranteed (it's to the right of the spread).

=== 3.14 Field Access

$
  "[FieldAccess]" quad (Gamma tack.r e => tau quad S(tau) tilde.equiv { ell : alpha | rho }) / (Gamma tack.r e.ell => alpha)
$

=== 3.15 Variant

$
  "[Variant]" quad (forall overline(alpha). (tau_"wrap" -> T overline(alpha)) = V(K) quad (tau'_w -> tau_"res") = "inst"(V(K)) quad Gamma tack.r e arrow.l tau'_w) / (Gamma tack.r K e => tau_"res")
$

A bare variant constructor $K$ (no payload expression) is treated as a first-class function
$tau_"wrap" -> T overline(alpha)$ if it wraps a type, or as a value of type $T overline(alpha)$ if it is a unit constructor.

=== 3.16 Let-In

$
  "[LetIn]" quad (Gamma tack.r e_1 => tau_1 quad p tack.r.double tau_1 => Delta quad Delta' = "Gen"(Gamma, Delta) quad Gamma, Delta' tack.r e_2 => tau_2) / (Gamma tack.r "let " p = e_1 " in " e_2 => tau_2)
$

== 4. Pattern Typing ($p tack.r.double tau => Delta$)

=== 4.1 Wildcard

$ "[PatWild]" quad "_" tack.r.double tau => emptyset $

=== 4.2 Literal Pattern

The pattern matches only if the scrutinee has the literal's base type; no bindings are produced:

$ "[PatLit]" quad (tau tilde.equiv tau_l quad tau_l in {"Num", "Text", "Bool"}) / (l tack.r.double tau => emptyset) $

=== 4.3 Variable Binding

A bare name binds the scrutinee's type directly:

$ "[PatIdent]" quad x tack.r.double tau => { x : tau } $

=== 4.4 Variant Pattern

$
  "[PatVariant]" quad ((tau_"wrap" -> T overline(beta)) = "inst"(V(K)) quad tau tilde.equiv T overline(beta) quad p tack.r.double tau_"wrap" => Delta) / (K p tack.r.double tau => Delta)
$

A wrapping variant matched without a sub-pattern (e.g. `Some` with no nested pattern)
ignores the wrapped value and produces no bindings:

$
  "[PatVariant-Bare]" quad ((tau_"wrap" -> T overline(beta)) = "inst"(V(K)) quad tau tilde.equiv T overline(beta)) / (K tack.r.double tau => emptyset)
$

=== 4.5 Record Pattern

Without a rest capture the pattern is exact — the scrutinee must have *only* the listed fields.
Each field may have an explicit sub-pattern (`ell : p`) or use name shorthand (`ell` alone
binds the field value to a variable named $ell$):

$
  "[PatRecord-Closed]" quad (tau tilde.equiv { ell_1 : alpha_1 , dots , ell_n : alpha_n | emptyset } quad p_i tack.r.double alpha_i => Delta_i) / ({ ell_1 : p_1 , dots , ell_n : p_n } tack.r.double tau => union.big Delta_i)
$

With a rest capture (`..rest` or bare `..`) the row tail is left open, allowing extra fields.
If a name is given it binds the remaining row as a record; bare `..` discards the rest:

$
  "[PatRecord-Open]" quad ("fresh" rho quad tau tilde.equiv { ell_1 : alpha_1 , dots , ell_n : alpha_n | rho } quad p_i tack.r.double alpha_i => Delta_i) / ({ ell_1 : p_1 , dots , ell_n : p_n , dots "rest" } tack.r.double tau => union.big Delta_i union { "rest" : { rho } })
$

=== 4.6 List Pattern

A list pattern unifies the scrutinee with $"List" alpha$; each element pattern is checked
against $alpha$. An optional rest captures the remaining $"List" alpha$; bare `..` (without
a name) discards the rest without binding:

$
  "[PatList]" quad ("fresh" alpha quad tau tilde.equiv "List" alpha quad p_i tack.r.double alpha => Delta_i quad "rest" : "List" alpha " (if named)") / ([p_1 , dots , p_n , dots "rest"] tack.r.double tau => union.big Delta_i union { "rest" : "List" alpha })
$

== 5. Checking Mode ($Gamma tack.r e arrow.l tau$)

The `check` function propagates a known expected type inward, avoiding unnecessary fresh
variables. It specializes four expression forms; all others fall back to synthesise-then-unify.

=== 5.1 Lambda (known function type)

$
  "[Chk-Lam]" quad (p tack.r.double tau_1 => Delta quad Gamma , Delta tack.r e arrow.l tau_2) / (Gamma tack.r (lambda p . e) arrow.l tau_1 -> tau_2)
$

If the expected type is not a function type, the rule falls back to synthesis.

=== 5.2 Record (known closed record type, fields only)

When the expected type is a known closed record and the record literal contains only
explicit fields (no spreads), each field's expected type is propagated:

$
  "[Chk-Record]" quad (tau = { ell_1 : tau_1 , dots , ell_n : tau_n | emptyset } quad Gamma tack.r e_i arrow.l tau_i quad (i in {1 dots n})) / (Gamma tack.r { ell_1 : e_1 , dots , ell_n : e_n } arrow.l tau)
$

Fields present in the literal but absent in the expected type are inferred rather than
checked; the resulting inferred record is unified with the expected type afterwards.

If the expected type is an open record, not a record at all, or the literal contains
spreads, the rule falls back to synthesis.

=== 5.3 Let-In (propagate to body)

$
  "[Chk-LetIn]" quad (Gamma tack.r e_1 => tau_1 quad p tack.r.double tau_1 => Delta quad Delta' = "Gen"(Gamma, Delta) quad Gamma , Delta' tack.r e_2 arrow.l tau) / (Gamma tack.r ("let " p = e_1 " in " e_2) arrow.l tau)
$

=== 5.4 Match Expression (propagate to arms)

$
  "[Chk-MatchExpr]" quad (Gamma tack.r e_s => tau_s quad forall i : p_i tack.r.double tau_s => Delta_i quad (Gamma , Delta_i tack.r g_i => "Bool") quad Gamma , Delta_i tack.r e_i arrow.l tau quad "exhaustive"(tau_s, overline(p))) / (Gamma tack.r ("match " e_s " | " overline(p -> e)) arrow.l tau)
$

=== 5.5 Bare Match Arms (known function type)

When a bare arm-list is checked against a known function type, the parameter type is
propagated to each pattern and the return type to each body:

$
  "[Chk-Match]" quad (forall i : p_i tack.r.double tau_1 => Delta_i quad (Gamma , Delta_i tack.r g_i => "Bool") quad Gamma , Delta_i tack.r e_i arrow.l tau_2 quad "exhaustive"(tau_1, overline(p))) / (Gamma tack.r (| p_1 -> e_1 | dots | p_n -> e_n) arrow.l tau_1 -> tau_2)
$

If the expected type is not a function type, the rule falls back to synthesis.

=== 5.6 Default (synthesise-then-unify)

For all other expression forms:

$ "[Chk-Default]" quad (Gamma tack.r e => tau' quad tau' tilde.equiv tau) / (Gamma tack.r e arrow.l tau) $

== 6. Traits and Operator Resolution

=== 6.1 Trait Definitions

A trait declaration introduces a type class with a single type parameter.
Method signatures are stored in `trait_env`:

$ "trait" T a { "let" m_i : tau_i } $

Methods may have operator names written in parenthesized form:
`trait Concat a { let (++) : a -> a -> a }`.

=== 6.2 Trait Implementations

An implementation provides concrete method definitions for a specific type or a parameterized
type family. The checker records the impl and its method types in `impl_env` or
`param_impl_env`:

$ "use" T "in" tau { "let" m_i = e_i } $

=== 6.3 User-Defined Operators

The lexer accepts arbitrary sequences of operator characters as a single token.
Any symbol sequence not already reserved (such as `<>`, `<$>`, `<*>`, `|>>`, etc.) becomes
a `Token::Operator(String)` and can be used as a binary operator at a fixed default
precedence. Operators are defined and used like Haskell type-class methods:

```
trait Mappable f {
  let (<$>) : (a -> b) -> f a -> f b
}
```

An operator in parentheses `(op)` is a first-class value expression that resolves to the
operator's binding (whether local or trait-derived). For example, `let f = (<$>)` binds `f`
to the same function as the `<$>` method.

=== 6.4 Operator Resolution

When a binary operator (e.g., `++` or a custom `<$>`) is encountered, `infer_operator_call`
resolves the operator type using the same logic as identifier resolution:

1. *Environment lookup* — if the operator name is bound in the local environment (via
  `let (++) = ...`), use that binding.
2. *Trait shorthand* — if exactly one trait defines a method with that name, instantiate
  the method type and record a trait constraint.
3. If zero or multiple traits match (without a local binding), report an unbound or
  ambiguous error.

The resolved type is applied as a curried function $tau_"op" tilde.equiv tau_1 -> tau_2 -> tau_r$.

=== 6.5 Operator as Value

The syntax `(op)` in expression position produces `ExprKind::Ident(op_name)`, which is
then resolved through the standard identifier/trait-method resolution. This allows passing
operators as higher-order arguments:

$ "[OpValue]" quad ("op" : sigma in Gamma union "traits" quad tau = "inst"(sigma)) / (Gamma tack.r ("op") => tau) $

=== 6.6 Constraint Checking

At the end of type checking, `check_trait_constraints` verifies that every recorded trait
constraint is satisfiable — i.e., that a matching impl exists for the resolved type parameter.
Constraints are accumulated during inference and checked *twice*:
1. After all top-level items have been processed (catches constraints from bindings/impls).
2. After the export expression is inferred (catches trait calls that appear only in `pub { }`).
Parameterized impls (e.g. `use ToText in List a`) are checked recursively — the checker
verifies that sub-constraints on the type parameters are also satisfiable.

== 7. Top-Level Bindings

=== 7.1 Self-Recursion

For top-level `let` bindings with a simple identifier pattern, a fresh monomorphic type
variable is inserted into the environment *before* inferring the body. This allows direct
self-recursion:

$
  "[LetRec]" quad ("fresh" alpha quad Gamma , { x : alpha } tack.r e => tau quad alpha tilde.equiv tau) / (Gamma tack.r "let " x = e => "Gen"(Gamma, tau))
$

=== 7.2 Type Annotations

Bindings may carry an explicit type annotation. The annotation is lowered to a `Ty` and the
body is *checked* against it rather than inferred:

$
  "[LetAnnot]" quad (tau_"ann" = "lower"(T) quad Gamma tack.r e arrow.l tau_"ann") / (Gamma tack.r "let " x : T = e => "Gen"(Gamma, tau_"ann"))
$

=== 7.3 Constraint Annotations

A binding may carry explicit trait constraints in addition to a type annotation.
Constraints of the form `(Trait1 a, Trait2 b) =>` restrict the type parameters
and are propagated into the resulting scheme without re-verification:

$
  "[LetConstrained]" quad (overline(C) => tau_"ann" = "lower"(T) quad Gamma tack.r e arrow.l tau_"ann") / (Gamma tack.r "let " x : overline(C) => T = e => forall overline(alpha) . overline(C) => tau_"ann")
$

=== 7.4 Mutual Recursion (Binding Groups)

Multiple bindings connected by `and` are type-checked as a mutual group via a
three-phase protocol:

$
  "[LetGroup]" quad ("Phase 1:" forall i : "fresh" alpha_i, quad Gamma' = Gamma , overline(x_i : alpha_i) \
  quad "Phase 2:" forall i : Gamma' tack.r e_i => tau_i, quad alpha_i tilde.equiv tau_i \
  quad "Phase 3:" forall i : sigma_i = "Gen"(Gamma, tau_i)) / (Gamma tack.r "let " x_1 = e_1 " and " dots " and " x_n = e_n => Gamma , overline(x_i : sigma_i))
$

Phase 1 inserts monomorphic placeholders. Phase 2 infers each body in the
extended environment (enabling mutual references). Phase 3 generalizes each
binding *with respect to the original environment* (not the extended one), so
that mutually-recursive references remain monomorphic within the group.

== 8. Type Definitions (ADTs)

=== 8.1 Algebraic Data Types

A type definition introduces a new type constructor and a set of variant
constructors:

$ "type" T overline(alpha) = | K_1 tau_1 | dots | K_n tau_n $

Each variant constructor is added to the variant environment.  A wrapping
variant $K_i$ with payload type $tau_i$ produces a constructor function:

$
  "[Ctor]" quad K_i : forall overline(alpha) . tau_i -> T overline(alpha)
$

A unit variant (no payload) produces a constant value:

$
  "[CtorUnit]" quad K_i : forall overline(alpha) . T overline(alpha)
$

The type parameters $overline(alpha)$ of the parent type are universally
quantified in each constructor's scheme.

=== 8.2 Exhaustiveness

When matching against a value of type $T overline(alpha)$, the checker looks up
all constructors $K_1 , dots , K_n$ of $T$ and verifies that non-guarded arms
cover every constructor. A wildcard or bare ident (non-guarded) counts as
covering all constructors.

== 9. Module System

=== 9.1 Imports

A use declaration makes another module's exports available in the current scope:

$ "use" x = "path" $

The imported module is type-checked (or its cached result is reused) and its
export type — a record type — is bound in the environment. Field access on the
import (`x.method`) follows standard field-access typing from Section 3.14.

=== 9.2 Destructuring Imports

A record-pattern import extracts specific fields from a module's export:

$ "use" { f_1, dots, f_n } = "path" $

Each $f_i$ is bound to the corresponding field type from the module's export
record scheme, instantiated independently.

=== 9.3 Module Exports

Every module produces an export type (the `pub { ... }` expression) which is
typed as a record. Modules without explicit exports produce the unit record
`{ }`. The export record's type scheme includes all constraints needed to use
the exported bindings.

=== 9.4 Prelude

Unless the module opts out via `-- lume no_prelude`, the standard prelude module
is implicitly imported before all user declarations.  It provides `map`,
`filter`, and the `Functor`, `ToText`, `Concat` traits among others.
