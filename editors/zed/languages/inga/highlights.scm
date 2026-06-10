; Generic fallbacks first — in Zed, later patterns take precedence.
(identifier) @variable
(type_identifier) @type

(comment) @comment

(string) @string
(escape_sequence) @string.escape
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)

(number) @number
(boolean) @boolean

[
  "struct"
  "enum"
  "service"
  "match"
  "catch"
  "fail"
  "provide"
  "uses"
  "lazy"
  "if"
  "else"
] @keyword

[
  "::"
  "->"
  "|>"
  "!"
  "?"
  "="
  "=="
  "!="
  "<"
  "<="
  ">"
  ">="
  "+"
  "-"
  "*"
  "/"
  "%"
  "&&"
  "||"
  "|"
] @operator

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  ","
  "."
] @punctuation.delimiter

; Declarations and calls.
(function_declaration name: (identifier) @function)
(method_signature name: (identifier) @function)
(implementation name: (identifier) @function)
(impl_field name: (identifier) @property)
(call_expression function: (identifier) @function)
(call_expression
  function: (field_expression field: (identifier) @function.method))
(field_expression field: (identifier) @property)
(parameter name: (identifier) @variable.parameter)
(acquire name: (identifier) @variable)
(enum_variant name: (type_identifier) @constructor)
(typed_pattern name: (identifier) @variable)

; Option constructors and builtin modules.
((type_identifier) @constructor
  (#match? @constructor "^(Some|None)$"))
((type_identifier) @namespace
  (#match? @namespace "^(Gfx|Schedule)$"))
