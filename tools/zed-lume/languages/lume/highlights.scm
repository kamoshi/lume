; Keywords
"let" @keyword
"type" @keyword
"use" @keyword
"if" @keyword.control
"then" @keyword.control
"else" @keyword.control
"and" @keyword.operator
"or" @keyword.operator

; Operators
"->" @operator
"|>" @operator
"?>" @operator
"|" @operator
"=" @operator
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

; Functions
(binding pattern: (pattern (identifier) @function))
(field_initializer . (identifier) @property)
(field_pattern . (identifier) @property)
(field_type . (identifier) @property)

; Field access
(field_access (identifier) @property .)

; Comments
(comment) @comment
