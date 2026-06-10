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
        $.error_declaration,
        $.type_declaration,
        $.service_declaration,
        $.function_declaration,
        $.implementation
      ),

    error_declaration: $ =>
      seq('error', field('name', $.type_identifier), '=', $.fields),

    type_declaration: $ =>
      seq('type', field('name', $.type_identifier), '=', $.fields),

    fields: $ => seq('{', repeat(seq($.field, optional(','))), '}'),

    field: $ => seq(optional($._type), field('name', $.identifier)),

    service_declaration: $ =>
      seq('service', field('name', $.type_identifier), '{', repeat($.method_signature), '}'),

    method_signature: $ => seq(field('name', $.identifier), '::', $.signature),

    function_declaration: $ =>
      seq(field('name', $.identifier), '::', $.signature, field('body', $.block)),

    implementation: $ =>
      seq(
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
        $.constructor_pattern,
        $.identifier,
        $.number,
        $.string,
        $.boolean,
        seq('-', $.number)
      ),

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

    provide_expression: $ =>
      seq('provide', sepBy1(',', $.identifier), field('body', $.block)),

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
