Based on the provided codebase for the Loxy language, the parser is implemented as a hybrid: it uses **Functional Recursive Descent** for high-level language constructs (like statements, blocks, control flow) and **Pratt Parsing (Top-Down Operator Precedence)** for binary and unary expressions. 

Here is a detailed breakdown of how the parser is organized and the algorithms it utilizes.

### 1. Code Organization
The parser's responsibilities are cleanly separated into three main files within the `crates/flox-core/src/parse/` directory:

* **`ast.rs` (The Output):** Defines the Abstract Syntax Tree. Interestingly, Loxy treats almost everything as an expression. There is no separate `Stmt` (Statement) enum; constructs like `Let`, `While`, `If`, and `Block` are all variants of the unified `Expr` enum.
* **`error.rs` (Diagnostics):** Defines `ParserError` and `ParserErrorKind`. It implements rich error reporting that can point to the exact line, offset, and length of a syntax error.
* **`parser.rs` (The Engine):** Contains the core parsing logic. It defines the mapping from grammatical rules (like `expr_if`, `expr_let`, `expr_while`) to recursive parsing functions.

### 2. The Core Algorithm: Functional Recursive Descent
Traditional recursive descent parsers often use a mutable global state (like a token iterator or a class property) to keep track of the current token being read. 

Loxy takes a distinct **functional approach** to state management. 

#### The Parsing Signature
Almost every parsing function in `parser.rs` shares the same fundamental signature:
```rust
fn parse_rule(ctx: &Context, tokens: &[Token]) -> Result<(usize, Box<Expr>), ParserError>
```

Instead of mutating a global pointer, the algorithm works by passing an immutable slice of the remaining `tokens`. If a function successfully matches a grammar rule, it returns a tuple containing:
1.  **`usize`:** The exact number of tokens it consumed.
2.  **`Box<Expr>`:** The resulting AST node.

#### Advancing the Parser
Because the functions return how many tokens they consumed, the parent function calling them uses a local `ptr` (pointer) variable to advance through the token slice. 

The algorithm follows this general pattern:
1. Initialize a local `ptr = 0`.
2. Pass a sub-slice of tokens starting at `ptr` (`&tokens[ptr..]`) to a child parsing function.
3. If successful, add the returned `consumed` count to `ptr`.
4. Repeat to parse subsequent parts of the grammar rule.
5. Finally, return the total `ptr` count and the combined AST node back up the call stack.

This organization eliminates hidden side-effects and makes it very easy to implement backtracking or lookahead, as you only commit to consuming tokens if the function returns `Ok`.

### 3. Grammar Routing and Sub-parsers
The parser functions map directly to the language's grammar rules. 

The entry point for individual expressions is the `expression` function, which acts as a router. It looks at the very first token in the provided slice and branches to the appropriate specialized sub-parser:
* If it sees `match`, it routes to `expr_match`.
* If it sees `let`, it routes to `expr_let`.
* If it sees `{`, it routes to `expr_block`.
* If it doesn't match a structural keyword, it falls through to assignment and mathematical expressions.

Utility functions like `consume` and `consume_ident` act as strict assertions. They check if the token at a specific offset matches an expected type (e.g., a closing parenthesis or an identifier). If it doesn't, they immediately generate a precise `ParserError`.

### 4. Expression Parsing: The Pratt Parser Integration
Recursive descent is notoriously clunky and inefficient at handling mathematical expressions with varying precedence (e.g., `1 + 2 * 3`) and associativity. To solve this, Loxy hands control over to a **Pratt Parser** (implemented in the `pratt` function) when dealing with operators.

#### The Algorithm
1.  **Context & Precedence:** The parser utilizes a `Context` struct that holds a registry of operators and their "binding powers" (precedence levels). For example, `*` has a higher binding power than `+`.
2.  **Prefix Parsing:** The `pratt` function first parses a prefix expression (which could be a unary operator like `-` or `!`, or a primary value like a number, string, or function call).
3.  **Infix Loop:** It then enters a `while` loop, checking the next token. If the next token is an operator, it checks the `Context` to see its binding power.
4.  **Binding Power Check:** If the operator's binding power is higher than the current context's minimum precedence, the parser recursively calls `pratt` to parse the right-hand side of the expression.
5.  **Tree Construction:** It combines the left-hand side, the operator, and the right-hand side into a `Binary` AST node and continues the loop.

### Summary
By combining functional state passing (via consumed token counts) with recursive descent for language structures and Pratt parsing for operator precedence, the Loxy parser achieves a clean, side-effect-free architecture that maps closely to standard language grammar definitions.
