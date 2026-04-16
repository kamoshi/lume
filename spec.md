# Lume language specification

> Expressive · Functional · Beginner-first

---

## 1. Philosophy

Lume is a small, functional language designed around four values:

- **Expressiveness** - say a lot with a little
- **Legibility** - code is for humans first
- **Predictability** - no surprises, no magic
- **Smallness** - a tiny core; everything else is library

Lume draws from Lua (small core, files as values), Elm and PureScript (row polymorphism, result types, immutability), and Haskell (type inference, pattern matching, pipelines). The sharp edges from each are filed off.

---

## 2. Lexical basics

```
-- single-line comment (no multi-line comments)
```

**Keywords:** `let` `pub` `type` `use` `trait` `in` `if` `then` `else` `match` `true` `false` `and` `or` `not`

**Identifiers:** `[a-z][a-zA-Z0-9_]*` for values and fields  
**Type names / variant names:** `[A-Z][a-zA-Z0-9]*`  
**Type variables:** single lowercase letters - `a`, `b`, `r`

**Literals:**

| Kind    | Example                        |
|---------|--------------------------------|
| Number  | `42`  `3.14`  `-7`            |
| Text    | `"hello"`  `"it's fine"`      |
| Bool    | `true`  `false`               |
| List    | `[1, 2, 3]`  `[]`            |
| Record  | `{ name: "Alice", age: 30 }` |

---

## 3. Bindings

```lume
-- immutable binding
let x = 42
let name = "Alice"

-- function binding (a function is just a value)
let double = n -> n * 2

-- multi-argument (curried by default)
let add = a -> b -> a + b

-- with optional type annotation
let greet : Text -> Text = name -> "Hello, " ++ name
```

All bindings are immutable. There is no assignment or mutation.

Lume is **expression-oriented** - everything evaluates to a value. There are no statements.

---

## 4. Functions

### 4.1 Application

```lume
double 5          -- 10
add 3 4           -- 7
```

Function application is left-associative and requires no parentheses for simple arguments. Parentheses group sub-expressions:

```lume
add (double 3) 4  -- 10
saveUser { name: "Alice" }
```

### 4.2 Pipelines

The `|>` operator passes the left-hand value as the last argument to the right-hand function:

```lume
5 |> double           -- 10
[1,2,3] |> map double -- [2,4,6]

-- chains read top-to-bottom
scores
  |> filter (s -> s >= 60)
  |> map    (s -> s * 1.05)
  |> average
```

### 4.3 Lambdas

```lume
n -> n * 2
a -> b -> a + b
{ name, .. } -> name
```

### 4.4 If expressions

```lume
if x > 0 then "positive" else "non-positive"

-- multi-line
if b == 0
  then Err { reason: "cannot divide by zero" }
  else Ok { value: a / b }
```

---

## 5. Built-in types

| Type   | Description              | Examples              |
|--------|--------------------------|-----------------------|
| `Num`  | 64-bit float             | `1`  `3.14`  `-7`   |
| `Text` | UTF-8 string             | `"hello"`            |
| `Bool` | Boolean                  | `true`  `false`      |
| `List a` | Homogeneous list       | `[1, 2, 3]`          |
| `Maybe a` | Optional value        | `Some { value: x }` / `None` |
| `Result a b` | Success or failure | `Ok { value: x }` / `Err { reason: e }` |

---

## 6. Records

### 6.1 Creation

```lume
let alice = { name: "Alice", age: 30, role: "admin" }

-- field shorthand: if variable name matches field name
let name = "Bob"
let bob = { name, age: 25 }   -- same as { name: name, age: 25 }
```

### 6.2 Access

```lume
alice.name   -- "Alice"
alice.age    -- 30
```

### 6.3 Update (immutable)

```lume
-- creates a new record; alice is unchanged
let older = { alice | age: 31 }
```

### 6.4 Row polymorphism

Functions can accept any record that has *at least* the required fields. The `..` in a type annotation means "and any other fields":

```lume
-- open row: accepts { name: Text } and anything else
let greet : { name: Text, .. } -> Text
let greet = { name, .. } -> "Hello, " ++ name

greet { name: "Alice", age: 30 }          -- works
greet { name: "Bob", role: "admin" }      -- works

-- closed row: accepts EXACTLY { name: Text, age: Num }
let strict : { name: Text, age: Num } -> Text
```

A function that adds a field:

```lume
let withScore : { name: Text, .. } -> { name: Text, score: Num, .. }
let withScore = rec -> { rec | score: 100 }
```

---

## 7. Sum types

### 7.1 Definition

```lume
-- unit variants (no payload)
type Direction =
  | North
  | South
  | East
  | West

-- variants with labelled record payloads
type Shape =
  | Circle   { radius: Num }
  | Rect     { width: Num, height: Num }
  | Triangle { base: Num, height: Num }

-- mixed
type Answer =
  | Yes
  | No
  | Maybe { reason: Text }

-- recursive
type Expr =
  | Num { value: Num }
  | Add { left: Expr, right: Expr }
  | Mul { left: Expr, right: Expr }
```

Variant payloads are always labelled records. There are no positional tuple variants.

### 7.2 Generic types

Type parameters are lowercase letters following the type name:

```lume
type Tree a =
  | Leaf
  | Node { value: a, left: Tree a, right: Tree a }

type Result a b =
  | Ok  { value: a }
  | Err { reason: b }

type Maybe a =
  | Some { value: a }
  | None
```

### 7.3 Construction

```lume
let c = Circle { radius: 5 }
let r = Rect   { width: 10, height: 4 }
let d = North            -- unit variant: no braces needed
let n = None

-- field shorthand works here too
let radius = 7
let c2 = Circle { radius }   -- same as Circle { radius: radius }
```

### 7.4 Derived behaviour

All sum types automatically support:

- **Structural equality** - `==` and `!=`
- **`show`** - human-readable string representation for debugging

```lume
North == North                              -- true
Circle { radius: 5 } == Circle { radius: 5 } -- true
show (Rect { width: 3, height: 4 })        -- "Rect { width: 3, height: 4 }"
```

---

## 8. Pattern matching

### 8.1 Syntax

Pattern matching uses `|` arms, consistent with type definitions:

```lume
let describe : Shape -> Text
let describe =
  | Circle   { radius }        -> "circle, r=" ++ show radius
  | Rect     { width, height } -> "rect " ++ show width ++ "x" ++ show height
  | Triangle { base, height }  -> "triangle"
```

Pattern matching supports guards and destructuring. Exhaustiveness checking is not currently guaranteed by this implementation.

### 8.2 Explicit match

The `match ... in` form matches an expression inline:

```lume
let label = match direction in
  | North -> "up"
  | South -> "down"
  | East  -> "right"
  | West  -> "left"
```

This is useful when the value being matched is not a function parameter.

### 8.3 The `..` rest pattern

`..` in a pattern means "and any other fields I don't care about":

```lume
-- bind only what you need
let classify =
  | { role: "admin", .. } -> "admin"
  | { age, .. } if age < 18 -> "minor"
  | { name, score, .. }   -> name ++ ": " ++ show score
  | _                     -> "unknown"
```

Without `..`, the pattern matches exactly those fields and no others.

### 8.4 Guards

```lume
let classify =
  | Circle { radius } if radius > 100 -> "huge"
  | Circle { radius } if radius > 10  -> "medium"
  | Circle _                          -> "small"
  | _                                 -> "not a circle"
```

`Variant _` matches any payload without binding it.

### 8.5 List patterns

```lume
let first =
  | []       -> Err { reason: "empty list" }
  | [x, ..]  -> Ok { value: x }

let second =
  | [_, x, ..] -> Ok { value: x }
  | _          -> Err { reason: "need at least two" }

-- bind the tail
let headTail =
  | [x, ..rest] -> Ok { head: x, tail: rest }
  | []           -> Err { reason: "empty" }
```

### 8.6 Destructuring in let bindings

The same patterns work in `let`:

```lume
let { name, age, .. } = alice
let { name: userName, .. } = alice
let { address: { city, .. }, .. } = alice

let [first, ..rest] = myList
```

---

## 9. Operators

| Operator | Meaning                              |
|----------|--------------------------------------|
| `\|>`    | Pipe - pass value into function      |
| `?>`     | Result pipe - pipe only if `Ok`      |
| `->`     | Lambda / function arrow              |
| `++`     | Concatenate (text, lists, records)   |
| `\|`     | Match arm / type variant separator   |
| `:`      | Type annotation                      |
| `==` `!=`| Structural equality                  |
| `<` `>` `<=` `>=` | Comparison (Num only)    |
| `+` `-` `*` `/` | Arithmetic (Num only)      |
| `and` `or` `not` | Boolean logic             |

`+` is **numbers only**. Text and list concatenation always uses `++`. This avoids the classic beginner footgun of `"5" + 3`.

### 9.1 The result pipe `?>`

`?>` chains operations that return `Result`, short-circuiting on the first `Err`:

```lume
let safeDivide = a -> b ->
  if b == 0
    then Err { reason: "division by zero" }
    else Ok { value: a / b }

10 |> safeDivide 2
   ?> (n -> Ok { value: n + 1 })   -- Ok { value: 6 }

10 |> safeDivide 0
   ?> (n -> Ok { value: n + 1 })   -- Err { reason: "division by zero" }
```

---

## 10. Type system

- **Inferred** - the compiler infers all types. Annotations are optional and serve as documentation.
- **Sound** - if the program compiles, it is type-correct. No runtime type errors.
- **Row polymorphic** - functions can be polymorphic over the "rest" of a record's fields (see §6.4).
- **Trait-constrained** - type annotations can require trait implementations (see §13).
- **No implicit coercions** - `Num` never becomes `Text` silently.
- **No `null` or `undefined`** - absence is represented by `Maybe`.

Type annotations use `:`:

```lume
let area : Shape -> Num
let greet : { name: Text, .. } -> Text
let withScore : { name: Text, .. } -> { name: Text, score: Num, .. }
let depth : Tree a -> Num
```

---

## 11. Modules

### 11.1 A module is a file with an optional `pub` export

```lume
-- math.lume

let pi = 3.14159
let area = r -> pi * r * r
let circumference = r -> 2 * pi * r

pub {
  area,
  circumference,
  pi,
}
```

Everything before `pub` is private. If a file omits `pub`, it implicitly exports the empty record `{}`.

Type declarations are module-local in the current implementation:

```lume
-- shapes.lume
type Shape =
  | Circle { radius: Num }
  | Rect   { width: Num, height: Num }

let area =
  | Circle { radius }        -> 3.14 * radius * radius
  | Rect   { width, height } -> width * height

pub { area }
```

### 11.2 Importing with `use`

```lume
-- bind the whole module as a record
use math = "./math"
math.area 5           -- 78.53

-- destructure on import
use { area, pi } = "./math"
area 5                -- 78.53

-- rename on import
use { area: circleArea } = "./math"
use { area: rectArea }   = "./geometry"

-- packages
use math = "lume:math"
use text = "lume:text"

-- relative paths
use utils = "./utils"
use cfg   = "../config"
```

`use` is a static declaration - always at the top of the file, never inside a function or branch.

### 11.3 Circular imports

Circular dependencies are a **hard compile error**. The compiler reports the full cycle:

```
Error: circular import detected
  main.lume  →  a.lume
  a.lume     →  b.lume
  b.lume     →  main.lume
```

Resolve by extracting the shared definitions into a third module that neither imports.

### 11.4 Re-exporting

A module can re-export selected bindings by importing them and publishing a new record:

```lume
use { area } = "./shapes"
use { pi } = "./math"

pub { area, pi }
```

---

## 12. Standard library (core)

The following are available globally - no import needed:

### Basics

| Function      | Type                        | Description                  |
|---------------|-----------------------------|------------------------------|
| `show`        | `a -> Text`                 | Convert any value to text    |
| `not`         | `Bool -> Bool`              | Boolean negation             |
| `max`         | `Num -> Num -> Num`         | Larger of two numbers        |
| `min`         | `Num -> Num -> Num`         | Smaller of two numbers       |
| `abs`         | `Num -> Num`                | Absolute value               |
| `round`       | `Num -> Num`                | Round to nearest integer     |
| `floor`       | `Num -> Num`                | Round down                   |
| `ceil`        | `Num -> Num`                | Round up                     |

### Lists

| Function      | Type                              | Description              |
|---------------|-----------------------------------|--------------------------|
| `map`         | `(a -> b) -> List a -> List b`   | Transform each element   |
| `filter`      | `(a -> Bool) -> List a -> List a`| Keep matching elements   |
| `fold`        | `b -> (b -> a -> b) -> List a -> b` | Reduce to a single value |
| `length`      | `List a -> Num`                   | Number of elements       |
| `reverse`     | `List a -> List a`                | Reverse a list           |
| `take`        | `Num -> List a -> List a`         | First n elements         |
| `drop`        | `Num -> List a -> List a`         | Skip first n elements    |
| `zip`         | `List a -> List b -> List { fst: a, snd: b }` | Pair up two lists |
| `any`         | `(a -> Bool) -> List a -> Bool`   | True if any match        |
| `all`         | `(a -> Bool) -> List a -> Bool`   | True if all match        |
| `average`     | `List Num -> Num`                 | Arithmetic mean          |
| `sum`         | `List Num -> Num`                 | Sum of elements          |
| `sort`        | `List Num -> List Num`            | Sort ascending           |
| `sortBy`      | `(a -> Num) -> List a -> List a`  | Sort by derived key      |

### Text

| Function      | Type                         | Description                   |
|---------------|------------------------------|-------------------------------|
| `trim`        | `Text -> Text`               | Remove surrounding whitespace |
| `split`       | `Text -> Text -> List Text`  | Split on delimiter            |
| `join`        | `Text -> List Text -> Text`  | Join with separator           |
| `contains`    | `Text -> Text -> Bool`       | Substring check               |
| `startsWith`  | `Text -> Text -> Bool`       | Prefix check                  |
| `endsWith`    | `Text -> Text -> Bool`       | Suffix check                  |
| `toUpper`     | `Text -> Text`               | Uppercase                     |
| `toLower`     | `Text -> Text`               | Lowercase                     |
| `length`      | `Text -> Num`                | Character count               |

### Result and Maybe

| Function      | Type                                              | Description               |
|---------------|---------------------------------------------------|---------------------------|
| `unwrap`      | `Result a e -> a`                                 | Extract Ok or crash       |
| `withDefault` | `a -> Maybe a -> a`                               | Extract Some or default   |
| `mapErr`      | `(e -> f) -> Result a e -> Result a f`            | Transform Err value       |
| `mapOk`       | `(a -> b) -> Result a e -> Result b e`            | Transform Ok value        |
| `mapMaybe`    | `(a -> b) -> Maybe a -> Maybe b`                  | Transform Some value      |
| `orElse`      | `Maybe a -> Maybe a -> Maybe a`                   | First Some wins           |
| `andThen`     | `(a -> Result b e) -> Result a e -> Result b e`   | Result chaining helper    |

---

## 13. Traits

Traits provide ad-hoc polymorphism — a way to define a shared interface that different types can implement independently. They are Lume's mechanism for overloading: the same function name (e.g. `show`) can behave differently depending on the type it is called with.

### 13.1 Defining a trait

A trait declares one or more method signatures parameterised over a type variable:

```lume
trait Show a {
  let show : a -> Text
}

trait Eq a {
  let eq : a -> a -> Bool
}
```

### 13.2 Implementing a trait

The `use ... in` form provides an implementation for a concrete type:

```lume
use Show in Num {
  let show = n -> showNum n
}

use Show in Bool {
  let show = | true  -> "true"
             | false -> "false"
}
```

Implementations can target applied types (type constructors applied to arguments):

```lume
type Box a = | MyBox { inner: a }

use Show in Box Num {
  let show = MyBox { inner } -> "MyBox(" ++ Show.show inner ++ ")"
}
```

### 13.3 Constrained implementations

An impl can require that its type parameter already implements another trait. Constraints appear before `=>`:

```lume
use Show in Show a => List a {
  let show = xs -> "[" ++ join ", " (map (x -> Show.show x) xs) ++ "]"
}
```

This says: "List a implements Show, provided a already implements Show." Multiple constraints are comma-separated:

```lume
use Printable in (Show a, Eq a) => Pair a {
  let display = p -> Show.show p
}
```

### 13.4 Calling trait methods

Use `Trait.method` syntax to call a trait method. The compiler resolves which implementation to use based on the argument type:

```lume
Show.show 42          -- uses Show in Num
Show.show [1, 2, 3]   -- uses Show in List a (which requires Show in Num)
```

### 13.5 Constrained functions

Functions can require trait implementations on their type parameters using constraint annotations:

```lume
let showBoth : (Show a) => a -> a -> Text
let showBoth = x -> y -> Show.show x ++ " and " ++ Show.show y
```

The constraint `(Show a) =>` means "this function works for any type `a` that has a `Show` implementation." Unparenthesized single constraints are also allowed:

```lume
let display : Show a => a -> Text
let display = x -> Show.show x
```

### 13.6 Soundness guarantees

The compiler enforces several rules at type-check time:

- **Missing impl:** calling `Show.show x` where `x` has a type with no `Show` impl is a compile error.
- **Incomplete impl:** an impl must provide all methods declared in the trait.
- **Extra methods:** an impl must not define methods not declared in the trait.
- **Duplicate impl:** two implementations for the same (trait, type) pair from different modules is a compile error. Diamond imports (same impl reaching a module via two paths) are allowed.

---

## 14. Error handling

Errors are values. There are no exceptions.

```lume
-- functions that can fail return Result
let safeDivide : Num -> Num -> Result Num Text
let safeDivide = a -> b ->
  if b == 0
    then Err { reason: "division by zero" }
    else Ok  { value: a / b }

-- handle with pattern matching
let result = safeDivide 10 2
| Ok  { value }  -> "got " ++ show value
| Err { reason } -> "failed: " ++ reason

-- or chain with ?>
safeDivide 10 2
  ?> ({ value } -> safeDivide value 2)
  ?> (value -> Ok { value: value + 1 })
-- Ok { value: 3.5 }
```

`Result` values are ordinary values. `?>` is the built-in operator for chaining computations that may fail.

---

## 15. Complete example

A small program that reads a list of quiz scores, filters and grades them, and summarises the results:

```lume
-- grader.lume

type Grade =
  | A | B | C | Fail

let toGrade : Num -> Grade
let toGrade =
  | s if s >= 90 -> A
  | s if s >= 75 -> B
  | s if s >= 60 -> C
  | _            -> Fail

let gradeLabel : Grade -> Text
let gradeLabel =
  | A    -> "A"
  | B    -> "B"
  | C    -> "C"
  | Fail -> "Fail"

let process : List { name: Text, score: Num, .. } -> List { name: Text, grade: Text }
let process = students ->
  students
    |> filter ({ score, .. } -> score >= 0)
    |> map    ({ name, score, .. } ->
                { name, grade: gradeLabel (toGrade score) })
    |> sortBy ({ grade, .. } -> grade)

pub { process, toGrade, gradeLabel }
```

```lume
-- main.lume
use { process } = "./grader"

let students =
  [ { name: "Alice", score: 93, year: 2 }
  , { name: "Bob",   score: 71, year: 3 }
  , { name: "Carol", score: 85, year: 2 }
  , { name: "Dan",   score: 55, year: 1 }
  ]

students
  |> process
  |> map ({ name, grade } -> name ++ ": " ++ grade)
  |> join "\n"
  |> show

-- Alice: A
-- Carol: B
-- Bob:   C
-- Dan:   Fail
```

---

## 16. What Lume intentionally omits

| Feature               | Reason omitted                                              |
|-----------------------|-------------------------------------------------------------|
| Mutation / `var`      | Immutability eliminates a class of bugs; use update syntax |
| `null` / `undefined`  | Use `Maybe` - absence is explicit and handled              |
| Exceptions            | Use `Result` - errors are values                           |
| Classes / inheritance | Row polymorphism + traits cover the use cases more simply |
| Macros / metaprogramming | Keeps the language predictable and tooling simple       |
| Concurrency primitives | Single-threaded; use packages for async I/O               |
| Operator overloading  | `++` for concat, `+` for numbers - unambiguous             |
| Implicit coercions    | All conversions are explicit                               |

---

## 17. Grammar summary

```
program     = use* (typedef | traitdef | impldef | binding)* ("pub" expr)?

use         = "use" (ident "=" | record_pattern "=") string
typedef     = "type" TypeName typevars "=" ("|" variant)+
variant     = VariantName record_type?

traitdef    = "trait" TypeName ident "{" trait_method* "}"
trait_method = "let" ident ":" type

impldef     = "use" TypeName "in" constraints? impl_type "{" impl_method* "}"
impl_type   = TypeName type_primary*
impl_method = "let" ident (":" type)? "=" expr
constraints = constraint ("," constraint)* "=>"
            | "(" constraint ("," constraint)* ")" "=>"
constraint  = TypeName ident

binding     = "let" pattern (":" constraints? type)? "=" expr

expr        = lambda | pipe_expr

lambda      = pattern "->" expr
pipe_expr   = result_pipe ("|>" result_pipe)*
result_pipe = apply ("?>" apply)*
apply       = atom atom*
            | apply record_expr
atom        = literal | ident | VariantName | trait_call
            | list_expr | "(" expr ")" | if_expr
            | match_expr | match_in_expr

trait_call    = TypeName "." ident       -- e.g. Show.show
if_expr       = "if" expr "then" expr "else" expr
match_expr    = ("|" pattern guard? "->" expr)+
match_in_expr = "match" expr "in" ("|" pattern guard? "->" expr)+
guard         = "if" expr

pattern     = "_"
            | literal
            | ident
            | VariantName pattern?
            | record_pattern
            | list_pattern

record_pattern = "{" (field_pattern ",")* (".." ident? )? "}"
field_pattern  = ident (":" pattern)?
list_pattern   = "[" (pattern ",")* (".." ident?)? "]"

type        = TypeName type*           -- applied type
            | ident                    -- type variable
            | record_type
            | type "->" type           -- function type

record_type = "{" (field_type ",")* ".."? "}"
field_type  = ident ":" type

typevars    = ident*
```

---

*Lume - version 0.1 draft*
