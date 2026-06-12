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
  "use"
  "pub"
  "struct"
  "enum"
  "service"
  "shared"
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

(record_update type: (type_identifier) @type)

; Option constructors and builtin functions.
((type_identifier) @constructor
  (#match? @constructor "^(Some|None)$"))
((identifier) @function.builtin
  (#match? @function.builtin "^(println|print|show|encode|decode|map|filter|fold|at|concat|reverse|split|slice|indexOf|trim|parseInt|toFloat|floor|getOrElse|orFail|retry|ignoreFailure|sleep|spawn|await|assert|assertEq|len|range|random|nowMillis|nowMicros)$"))
