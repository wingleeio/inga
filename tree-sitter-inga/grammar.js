// Tree-sitter grammar for Inga, used by the Zed extension for syntax
// highlighting. Permissive by design: newlines are treated as whitespace
// (statement boundaries fall out of the expression grammar), which is enough
// for highlighting and structural queries.

const PREC = {
  pipe: 1,
  or: 2,
  and: 3,
  compare: 4,
  add: 5,
  multiply: 6,
  unary: 7,
  call: 9,
  field: 10,
};

function sepBy1(sep, rule) {
  return seq(rule, repeat(seq(sep, rule)), optional(sep));
}

module.exports = grammar({
  name: 'inga',

  extras: $ => [/\s/, $.comment],

  word: $ => $.identifier,

  conflicts: $ => [
    [$._type, $.acquire, $._expression],
    [$._type, $._expression],
    [$.parameter, $._expression],
    [$.provide_item],
  ],

  rules: {
    source_file: $ => repeat($._declaration),

    comment: $ =>
      token(
        choice(
          seq('//', /[^\n]*/),
          seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/')
        )
      ),

    // ---- declarations ----------------------------------------------------

    _declaration: $ =>
      choice(
        $.use_declaration,
        $.struct_declaration,
        $.enum_declaration,
        $.service_declaration,
        $.function_declaration,
        $.implementation
      ),

    // `use Gfx` (std module) or `use geometry` (sibling file).
    use_declaration: $ =>
      seq('use', field('module', choice($.identifier, $.type_identifier))),

    struct_declaration: $ =>
      seq(optional('pub'), 'struct', field('name', $.type_identifier), '=', $.fields),

    // `enum Shape = Circle { Float radius } | Rect { Float w, Float h } | Dot`
    // A newline may precede each `|`, which works out because newlines are
    // treated as whitespace.
    enum_declaration: $ =>
      seq(
        optional('pub'),
        'enum',
        field('name', $.type_identifier),
        '=',
        $.enum_variant,
        repeat(seq('|', $.enum_variant))
      ),

    enum_variant: $ =>
      seq(field('name', $.type_identifier), optional($.fields)),

    fields: $ => seq('{', repeat(seq($.field, optional(','))), '}'),

    field: $ => seq(optional($._type), field('name', $.identifier)),

    service_declaration: $ =>
      seq(optional('pub'), 'service', field('name', $.type_identifier), '{', repeat($.method_signature), '}'),

    method_signature: $ => seq(field('name', $.identifier), '::', $.signature),

    function_declaration: $ =>
      seq(optional('pub'), field('name', $.identifier), '::', $.signature, field('body', $.block)),

    implementation: $ =>
      seq(
        optional('pub'),
        field('name', $.identifier),
        '::',
        field('service', $.type_identifier),
        '{',
        repeat(choice($.function_declaration, $.impl_field)),
        '}'
      ),

    impl_field: $ => seq(field('name', $.identifier), '=', $._expression),

    signature: $ =>
      seq(
        $.parameters,
        optional(seq('->', $._type)),
        optional($.error_row),
        optional($.uses_row)
      ),

    parameters: $ => seq('(', optional(sepBy1(',', $.parameter)), ')'),

    parameter: $ =>
      seq(optional('lazy'), optional($._type), field('name', $.identifier)),

    error_row: $ => seq('!', sepBy1(',', $.type_identifier)),

    uses_row: $ => seq('uses', sepBy1(',', $.type_identifier)),

    // ---- types -------------------------------------------------------------

    _type: $ => choice($.type_identifier, $.list_type, $.option_type),

    list_type: $ => seq('[', $._type, ']'),

    option_type: $ => prec(PREC.field, seq($._type, '?')),

    // ---- statements ----------------------------------------------------------

    block: $ => seq('{', repeat($._statement), '}'),

    _statement: $ => choice($.let_binding, $.acquire, $._expression),

    let_binding: $ =>
      seq(optional($._type), field('name', $.identifier), '=', $._expression),

    // `Cache cache` — bind a capability from the environment.
    acquire: $ =>
      seq(field('service', $.type_identifier), field('name', $.identifier)),

    // ---- expressions ------------------------------------------------------------

    _expression: $ =>
      choice(
        $.pipe_expression,
        $.binary_expression,
        $.unary_expression,
        $.call_expression,
        $.field_expression,
        $.match_expression,
        $.if_expression,
        $.fail_expression,
        $.provide_expression,
        $.lambda,
        $.list,
        $.block,
        $.paren_expression,
        $.string,
        $.number,
        $.boolean,
        $.identifier,
        $.type_identifier
      ),

    pipe_expression: $ =>
      prec.left(
        PREC.pipe,
        seq($._expression, '|>', choice($.catch_clause, $._expression))
      ),

    catch_clause: $ => seq('catch', '{', repeat($.arm), '}'),

    binary_expression: $ =>
      choice(
        prec.left(PREC.or, seq($._expression, '||', $._expression)),
        prec.left(PREC.and, seq($._expression, '&&', $._expression)),
        prec.left(
          PREC.compare,
          seq($._expression, choice('==', '!=', '<', '<=', '>', '>='), $._expression)
        ),
        prec.left(PREC.add, seq($._expression, choice('+', '-'), $._expression)),
        prec.left(PREC.multiply, seq($._expression, choice('*', '/', '%'), $._expression))
      ),

    unary_expression: $ =>
      prec(PREC.unary, seq(choice('-', '!'), $._expression)),

    call_expression: $ =>
      prec(PREC.call, seq(field('function', $._expression), field('arguments', $.arguments))),

    arguments: $ => seq('(', optional(sepBy1(',', $._expression)), ')'),

    field_expression: $ =>
      prec.left(PREC.field, seq($._expression, '.', field('field', $.identifier))),

    match_expression: $ =>
      seq('match', field('value', $._expression), '{', repeat($.arm), '}'),

    arm: $ => seq($._pattern, '->', $._expression, optional(',')),

    _pattern: $ =>
      choice(
        $.typed_pattern,
        $.constructor_pattern,
        $.identifier,
        $.number,
        $.string,
        $.boolean,
        seq('-', $.number)
      ),

    // `String reason -> ...` — a type name followed by a binder; matches a
    // value of that type and binds it.
    typed_pattern: $ =>
      seq(field('type', $.type_identifier), field('name', $.identifier)),

    constructor_pattern: $ =>
      seq(
        field('name', $.type_identifier),
        optional(choice($.pattern_arguments, $.pattern_fields))
      ),

    pattern_arguments: $ => seq('(', optional(sepBy1(',', $._pattern)), ')'),

    pattern_fields: $ => seq('{', optional(sepBy1(',', $.identifier)), '}'),

    if_expression: $ =>
      seq(
        'if',
        field('condition', $._expression),
        field('then', $.block),
        optional(seq('else', field('else', choice($.if_expression, $.block))))
      ),

    fail_expression: $ => prec.right(seq('fail', $._expression)),

    // `provide a, b { ... }` (braced: scopes over the block) or
    // `provide Arena(256.kb), logger` (braceless: scopes over the rest of the
    // enclosing block). Items may be plain bindings or calls with arguments.
    provide_expression: $ =>
      prec.right(
        seq('provide', sepBy1(',', $.provide_item), optional(field('body', $.block)))
      ),

    provide_item: $ =>
      seq(
        choice($.identifier, $.type_identifier),
        optional(field('arguments', $.arguments))
      ),

    lambda: $ =>
      prec.right(seq($.parameters, '->', field('body', $._expression))),

    list: $ => seq('[', optional(sepBy1(',', $._expression)), ']'),

    paren_expression: $ => seq('(', $._expression, ')'),

    // ---- literals --------------------------------------------------------------

    string: $ =>
      seq(
        '"',
        repeat(choice($.escape_sequence, $.interpolation, token.immediate(prec(1, /[^"\\$]+/)), '$')),
        '"'
      ),

    escape_sequence: $ => token.immediate(/\\[ntr0"\\$]/),

    interpolation: $ => seq('${', $._expression, '}'),

    number: $ => /\d[\d_]*(\.\d[\d_]*)?/,

    boolean: $ => choice('true', 'false'),

    identifier: $ => /[a-z_][A-Za-z0-9_]*/,

    type_identifier: $ => /[A-Z][A-Za-z0-9_]*/,
  },
});
