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
  "type"
  "enum"
  "service"
  "provider"
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
(value_member name: (identifier) @property)
(provider_declaration name: (identifier) @function)
(provider_declaration name: (type_identifier) @function)
(call_expression function: (identifier) @function)
(call_expression
  function: (field_expression field: (identifier) @function.method))
(field_expression field: (identifier) @property)
(parameter name: (identifier) @variable.parameter)
(acquire name: (identifier) @variable)
(enum_variant name: (type_identifier) @constructor)
(typed_pattern name: (identifier) @variable)

(type_alias_declaration name: (type_identifier) @type)
(record_update type: (type_identifier) @type)
(field_initializer field: (identifier) @property)
(interpolation type: (type_identifier) @type)
(interpolation name: (identifier) @variable)

; Option constructors and builtin functions.
((type_identifier) @constructor
  (#match? @constructor "^(Some|None)$"))
((identifier) @function.builtin
  (#match? @function.builtin "^(println|print|show|readLine|map|filter|fold|at|concat|reverse|split|join|contains|startsWith|endsWith|replace|toUpper|toLower|sort|sortBy|min|max|abs|bitAnd|bitOr|bitXor|bitNot|shiftL|shiftR|byteAt|byteLen|intToBytes|bytesToInt|fromBytes|slice|indexOf|trim|parseInt|toFloat|floor|getOrElse|orFail|retry|ignoreFailure|tap|tapError|then|sleep|assert|assertEq|len|range|random|nowMillis|nowMicros)$"))
