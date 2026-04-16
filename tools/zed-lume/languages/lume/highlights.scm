; Keywords
"let" @keyword
"type" @keyword
"use" @keyword
"pub" @keyword
"and" @keyword
"trait" @keyword
"in" @keyword

"if" @keyword.control
"then" @keyword.control
"else" @keyword.control
"match" @keyword.control

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
(trait_method name: (identifier) @function)

; Impl (instance) definitions
(impl_definition trait: (type_identifier) @type)
(impl_definition type_name: (type_identifier) @type)
(impl_method name: (identifier) @function)

; Constraint annotations — trait name in `(ToText a, ToText b) =>`
(constraint trait: (type_identifier) @type)

; Record fields
(field_initializer name: (identifier) @property)
(field_initializer name: (type_identifier) @type)
(field_pattern name: (identifier) @property)
(field_pattern name: (type_identifier) @type)
(field_type name: (identifier) @property)

; Field access — the accessed name
(field_access (identifier) @property .)

; Comments
(comment) @comment
