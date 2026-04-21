; Keywords
"let" @keyword
"type" @keyword
"use" @keyword
"pub" @keyword
"and" @keyword
"trait" @keyword
"in" @keyword
"match" @keyword.control

"if" @keyword.control
"then" @keyword.control
"else" @keyword.control

"infix" @keyword
"infixl" @keyword
"infixr" @keyword

; Operators
"->" @operator
"|>" @operator
"?>" @operator
"&&" @operator
"||" @operator
"|" @operator
"=" @operator
"=>" @operator
"++" @operator
"+" @operator
"-" @operator
"*" @operator
"/" @operator
"==" @operator
"!=" @operator
"<" @operator
">" @operator
"<=" @operator
">=" @operator
".." @operator
":" @punctuation.delimiter

; Literals
(string) @string
(number) @number
(bool) @boolean

; Identifiers
(identifier) @variable
(type_identifier) @type

; Top-level and let bindings — name is a function
(binding pattern: (pattern (identifier) @function))

; Trait definitions
(trait_definition name: (type_identifier) @type)
(trait_definition param: (identifier) @type.parameter)
(trait_method name: (identifier) @function)

; Impl (instance) definitions
(impl_definition trait: (type_identifier) @type)
(impl_target_type name: (type_identifier) @type)
(impl_target_type arg: (type_identifier) @type)
(impl_target_type arg: (identifier) @type.parameter)
(impl_method name: (identifier) @function)

; Impl constraint annotations — trait name in `Show a => List a { ... }`
(impl_constraint_list (constraint trait: (type_identifier) @type))
(impl_constraint_list (constraint var: (identifier) @type.parameter))

; Constraint annotations — trait name in `(ToText a, ToText b) =>`
(constraint trait: (type_identifier) @type)
(constraint var: (identifier) @type.parameter)

; Record fields
(field_initializer name: (identifier) @property)
(field_initializer name: (type_identifier) @type)
(field_pattern name: (identifier) @property)
(field_pattern name: (type_identifier) @type)
(field_type name: (identifier) @property)

; Field access — the accessed name
(field_access (identifier) @property .)

; Comments
(doc_comment) @comment.doc
(comment) @comment

; Typed hole
(hole) @variable.special

; Unary operators
"not" @keyword.operator
