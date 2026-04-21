; ── Explicit bracket delimiters ───────────────────────────────────────────────

(record_expr "{" @indent)
(record_expr "}" @dedent)
(record_type "{" @indent)
(record_type "}" @dedent)
(record_pattern "{" @indent)
(record_pattern "}" @dedent)
(trait_definition "{" @indent)
(trait_definition "}" @dedent)
(impl_definition "{" @indent)
(impl_definition "}" @dedent)
(list_expr "[" @indent)
(list_expr "]" @dedent)
(list_pattern "[" @indent)
(list_pattern "]" @dedent)
(paren_expr "(" @indent)
(paren_expr ")" @dedent)
(paren_type "(" @indent)
(paren_type ")" @dedent)

; ── Let bindings ───────────────────────────────────────────────────────────────

; Body after = is indented.
(binding "=" @indent)

; Typing `let` for a new top-level binding dedents back to base level.
; Only fires for `binding` nodes (top-level), not `let_in_expr`.
(binding "let" @dedent)
(type_definition "type" @dedent)
(trait_definition "trait" @dedent)
(impl_definition "use" @dedent)
(use_declaration "use" @dedent)
(program "pub" @dedent)

; ── Match expressions ──────────────────────────────────────────────────────────

; `match x in` — arms are indented after `in`.
(match_in_expr "in" @indent)

; Each arm body (after `->`) is indented one further level.
(match_arm "->" @indent)

; Typing `|` to start the next arm snaps back to the arm level
; (dedents from the previous arm's body).
(match_arm "|" @dedent)
