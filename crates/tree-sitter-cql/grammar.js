// tree-sitter grammar for CQL (Churcuring Query Language).
//
// Grammar authority: doc/cql.md appendix A (revised). This file implements:
//  - A.1 lexical rules (comments, identifiers, int/float/string literals
//    with escapes and `\(...)` interpolation, keywords, longest-match symbols)
//  - A.2 declarations, types, expressions (8 precedence levels, stratified so
//    that comparison operators are non-chainable), patterns, temporal
//    expressions for `property` bodies.
//
// Generated with tree-sitter CLI 0.26.x; run `tree-sitter generate`.

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// Precedence levels for expressions (low -> high), mirroring A.2.
const PREC = {
  IMPL: 1, // =>       (right)
  OR: 2, //   \/       (left)
  AND: 3, //  /\       (left)
  CMP: 4, //  = /= < > <= >= \in \subseteq  (non-associative)
  ADD: 5, //  + - \cup \                    (left)
  MUL: 6, //  * / % \cap \X                 (left)
  UNARY: 7, // ~ - (prefix)
  CAST: 8, // `as` type
  POSTFIX: 9, // . ( ) ?
  PRIME: 10, // ' (next-state, property bodies)
  FUN_TYPE: 1, // -> in types (right)
};

// Temporal operators inside `property` bodies.
const TEMPORAL_PREC = {
  UNTIL: 1, // until  (right, lower than ~>)
  LEADS_TO: 2, // ~> (right)
  PREFIX: 3, // [] <>
};

/** Comma-separated, at least one, optional trailing comma. */
function commaSep1(rule) {
  return seq(rule, repeat(seq(',', rule)), optional(','));
}

/** Comma-separated, zero or more, optional trailing comma. */
function commaSep(rule) {
  return optional(commaSep1(rule));
}

module.exports = grammar({
  name: 'cql',

  word: $ => $.ident,

  extras: $ => [$.comment, /\s/],

  supertypes: $ => [$._expression],

  conflicts: $ => [
    // A top-level expression vs the same expression inside the unary/postfix
    // levels of the comparison sub-hierarchy: GLR split; the sub-hierarchy
    // reading dies unless a comparison operator actually follows.
    [$._expression, $._postfix_level],
    [$._expression, $._unary_level],
    // `set { none \in S if ... }` / `set { some(p) \in ... }`: `none` and
    // `some(...)` are both option literals (expressions) and option patterns.
    [$.option_literal, $.option_pattern],
    // Literals (`1`, `"s"`, `true`) are also valid patterns, so a set/filter
    // form starting with a literal is ambiguous until `\in`/`if` resolves it.
    [$.pattern, $.literal],
    // `set { x \in S ... }`: `x` as comparison operand vs generator pattern.
    [$._postfix_level, $.pattern],
    // `set { f(x) \in S if ... }`: `f(x)` as call expression (set element)
    // vs variant pattern (filter form generator).
    [$._postfix_level, $.variant_pattern],
    // `set { {x} \in S if ... }`: `{x}` as block (expression) vs record
    // pattern in a generator.
    [$._expression, $.record_pattern],
    // `set { (x, y) \in S if ... }`: `(x, ...)` as tuple/parenthesized
    // expression vs tuple pattern in a generator.
    [$._expression, $.pattern],
    // `set { [] \in S if ... }`: `[]` as empty vector literal vs empty
    // vector pattern in a generator.
    [$.vector_literal, $.vector_pattern],
    // `x as Foo < T >` (cast to generic type) vs `(x as Foo) < y` (cast then
    // comparison); the losing branch always dies (`>` unmatched or chained
    // comparison), so GLR resolves it.
    [$.named_type],
  ],

  rules: {
    // ------------------------------------------------------------------
    // Source file & declarations
    // ------------------------------------------------------------------

    source_file: $ => seq(
      'module',
      field('name', $.ident),
      optional(';'),
      repeat($._item),
    ),

    _item: $ => choice(
      $.use_declaration,
      $.const_declaration,
      $.type_declaration,
      $.enum_declaration,
      $.table_declaration,
      $.index_declaration,
      $.function_declaration,
      $.query_declaration,
      $.action_declaration,
      $.invariant_declaration,
      $.test_declaration,
      $.property_declaration,
      $.fairness_declaration,
    ),

    visibility: $ => 'public',

    use_declaration: $ => seq(
      'use',
      $.ident,
      repeat(seq('::', $.ident)),
      optional(seq('as', field('alias', $.ident))),
      optional(';'),
    ),

    const_declaration: $ => seq(
      optional($.visibility),
      'const',
      field('name', $.ident),
      ':',
      field('type', $.type),
      '==',
      field('value', $._expression),
      optional(';'),
    ),

    type_declaration: $ => seq(
      optional($.visibility),
      'type',
      field('name', $.ident),
      optional(field('type_parameters', $.type_parameters)),
      '==',
      field('definition', $.type),
      optional(';'),
    ),

    enum_declaration: $ => seq(
      optional($.visibility),
      'enum',
      field('name', $.ident),
      optional(field('type_parameters', $.type_parameters)),
      '{',
      commaSep1($.variant),
      '}',
    ),

    variant: $ => seq(
      field('name', $.ident),
      optional(choice(
        seq('(', commaSep1($.type), ')'),
        field('payload', $.record_type),
      )),
    ),

    table_declaration: $ => seq(
      optional($.visibility),
      'table',
      field('name', $.ident),
      field('schema', $.record_type),
      $.primary_key_clause,
      repeat($.foreign_key_clause),
      optional(';'),
    ),

    primary_key_clause: $ => seq(
      'primary', 'key', '{', commaSep1($.ident), '}',
    ),

    foreign_key_clause: $ => seq(
      'foreign', 'key', '{', commaSep1($.ident), '}',
      'references', field('target', $.ident),
    ),

    index_declaration: $ => seq(
      optional($.visibility),
      'index',
      field('name', $.ident),
      'on',
      field('table', $.ident),
      '(',
      commaSep1($.ident),
      ')',
      optional(';'),
    ),

    function_declaration: $ => seq(
      optional($.visibility),
      'function',
      optional('recursive'),
      field('name', $.ident),
      optional(field('type_parameters', $.type_parameters)),
      '(',
      optional($.parameters),
      ')',
      '->',
      field('return_type', $.type),
      optional($._suffix),
      optional(seq('==', field('body', $.block))),
      optional(';'),
    ),

    query_declaration: $ => seq(
      optional($.visibility),
      'query',
      optional('recursive'),
      field('name', $.ident),
      '(',
      optional($.parameters),
      ')',
      '->',
      field('return_type', $.type),
      optional($._suffix),
      '==',
      field('body', $.block),
    ),

    action_declaration: $ => seq(
      optional($.visibility),
      'action',
      optional('recursive'),
      field('name', $.ident),
      '(',
      optional($.parameters),
      ')',
      '->',
      'set', '<', 'write_op', '>',
      optional($._suffix),
      '==',
      field('body', $.block),
    ),

    _suffix: $ => choice(
      seq($.decreases_clause, optional($.depth_clause)),
      $.depth_clause,
    ),

    decreases_clause: $ => seq('decreases', field('measure', $.ident)),

    depth_clause: $ => seq('with', 'depth', field('bound', $.int_literal)),

    invariant_declaration: $ => seq(
      'invariant',
      field('name', $.ident),
      'on',
      field('table', $.ident),
      '==',
      field('condition', $._expression),
      optional(';'),
    ),

    test_declaration: $ => seq(
      'test',
      field('name', $.ident),
      '{',
      repeat($._test_statement),
      '}',
    ),

    _test_statement: $ => choice(
      $.fixture_statement,
      $.expect_statement,
    ),

    fixture_statement: $ => seq(
      'fixture',
      field('name', $.ident),
      '==',
      field('value', $.vector_literal),
      optional(';'),
    ),

    expect_statement: $ => seq(
      'expect',
      field('actual', $._expression),
      '==',
      field('expected', $._expression),
      optional(';'),
    ),

    property_declaration: $ => seq(
      'property',
      field('name', $.ident),
      '==',
      field('body', $.temporal_expression),
      optional(';'),
    ),

    fairness_declaration: $ => seq(
      'fairness',
      field('kind', choice('weak', 'strong')),
      '==',
      commaSep1($.ident),
      optional(';'),
    ),

    type_parameters: $ => seq('<', commaSep1($.ident), '>'),

    parameters: $ => commaSep1($.parameter),

    parameter: $ => seq(
      field('name', $.ident),
      ':',
      field('type', $.type),
    ),

    // ------------------------------------------------------------------
    // Types
    // ------------------------------------------------------------------

    type: $ => choice(
      $.primitive_type,
      $.decimal_type,
      $.named_type,
      $.key_type,
      $.value_type,
      $.option_type,
      $.vector_type,
      $.set_type,
      $.bag_type,
      $.map_type,
      $.table_type,
      $.tuple_type,
      $.function_type,
      $.record_type,
    ),

    primitive_type: $ => choice('bool', 'int', 'float', 'string', 'date'),

    decimal_type: $ => prec.right(seq(
      'decimal',
      optional(seq('(', $.int_literal, ',', $.int_literal, ')')),
    )),

    named_type: $ => seq(
      field('name', $.ident),
      optional(field('type_arguments', $.type_arguments)),
    ),

    key_type: $ => seq('key', field('table', $.ident)),

    value_type: $ => seq('value', field('table', $.ident)),

    option_type: $ => seq('option', '<', field('element', $.type), '>'),

    vector_type: $ => seq('vector', '<', field('element', $.type), '>'),

    set_type: $ => seq('set', '<', field('element', $.type), '>'),

    bag_type: $ => seq('bag', '<', field('element', $.type), '>'),

    map_type: $ => seq(
      'map', '<',
      field('key', $.type), ',',
      field('value', $.type),
      '>',
    ),

    table_type: $ => seq(
      'table', '<', '(',
      field('key', $.type), ',',
      field('value', $.type),
      ')', '>',
    ),

    tuple_type: $ => seq(
      '(',
      $.type, ',',
      commaSep1($.type),
      ')',
    ),

    function_type: $ => prec.right(PREC.FUN_TYPE, seq(
      field('parameter', choice($.type, seq('(', $.type, ')'))),
      '->',
      field('return_type', $.type),
    )),

    record_type: $ => seq('{', commaSep1($.field_declaration), '}'),

    field_declaration: $ => seq(
      field('name', $.ident),
      ':',
      field('type', $.type),
    ),

    type_arguments: $ => seq('<', commaSep1($.type), '>'),

    // ------------------------------------------------------------------
    // Temporal expressions (property bodies)
    // ------------------------------------------------------------------

    temporal_expression: $ => choice(
      $.always_expression,
      $.eventually_expression,
      $.leads_to_expression,
      $.until_expression,
      $._expression,
    ),

    // `[]` is matched as two tokens so that an empty vector literal `[]`
    // can still be lexed; the ambiguity is resolved by dynamic precedence
    // in favour of the temporal reading (see `conflicts`).
    always_expression: $ => prec.dynamic(1, prec.right(TEMPORAL_PREC.PREFIX, seq(
      '[', ']',
      field('operand', $.temporal_expression),
    ))),

    eventually_expression: $ => prec.right(TEMPORAL_PREC.PREFIX, seq(
      '<>',
      field('operand', $.temporal_expression),
    )),

    leads_to_expression: $ => prec.right(TEMPORAL_PREC.LEADS_TO, seq(
      field('left', $.temporal_expression),
      '~>',
      field('right', $.temporal_expression),
    )),

    until_expression: $ => prec.right(TEMPORAL_PREC.UNTIL, seq(
      field('left', $.temporal_expression),
      'until',
      field('right', $.temporal_expression),
    )),

    // ------------------------------------------------------------------
    // Expressions (stratified by precedence level; see A.2)
    // ------------------------------------------------------------------

    // Binary operators: the five associative levels (=> \/ /\ additive
    // multiplicative) are one flat rule resolved by `prec` (see PREC).
    // Comparison is a separate rule whose operands are additive
    // expressions, so `a = b = c` is a hard syntax error (non-chainable,
    // per A.2).
    _expression: $ => choice(
      $.binary_expression,
      $.comparison_expression,
      $.unary_expression,
      $.cast_expression,
      $.member_expression,
      $.call_expression,
      $.try_expression,
      $.primed_expression,
      $.parenthesized_expression,
      $.tuple_literal,
      $.vector_literal,
      $.block,
      $.if_expression,
      $.match_expression,
      $.set_form,
      $.bag_form,
      $.map_literal,
      $.record_literal,
      $.record_update,
      $.quantifier,
      $.option_literal,
      $.lambda,
      $.literal,
      $.ident,
    ),

    binary_expression: $ => {
      const table = [
        [prec.right, PREC.IMPL, '=>'],
        [prec.left, PREC.OR, '\\/'],
        [prec.left, PREC.AND, '/\\'],
        [prec.left, PREC.ADD, $._add_operator],
        [prec.left, PREC.MUL, $._mul_operator],
      ];
      return choice(
        ...table.map(([assoc, precedence, operator]) => assoc(precedence, seq(
          field('left', $._expression),
          field('operator', operator),
          field('right', $._expression),
        ))),
      );
    },

    comparison_expression: $ => prec(PREC.CMP, seq(
      field('left', $.additive_expression),
      field('operator', $._cmp_operator),
      field('right', $.additive_expression),
    )),

    _cmp_operator: $ => choice(
      '=', '/=', '<', '>', '<=', '>=', '\\in', '\\subseteq',
    ),

    _add_operator: $ => choice('+', '-', '\\cup', '\\'),

    _mul_operator: $ => choice('*', '/', '%', '\\cap', '\\X'),

    // Additive/multiplicative levels, reachable as comparison operands
    // (where the flat `binary_expression` cannot be used, since it would
    // allow chaining) and mirroring the ADD/MUL levels elsewhere.
    additive_expression: $ => choice(
      prec.left(PREC.ADD, seq(
        field('left', $.additive_expression),
        field('operator', $._add_operator),
        field('right', $.multiplicative_expression),
      )),
      $.multiplicative_expression,
    ),

    multiplicative_expression: $ => choice(
      prec.left(PREC.MUL, seq(
        field('left', $.multiplicative_expression),
        field('operator', $._mul_operator),
        field('right', $._unary_level),
      )),
      $._unary_level,
    ),

    // Unary-and-below level: everything except binary operators. Shared by
    // unary operands and by the comparison sub-hierarchy.
    _unary_level: $ => choice(
      $.unary_expression,
      $.cast_expression,
      $._postfix_level,
    ),

    // Postfix level: primaries plus the postfix forms. Operands of the
    // postfix/cast rules are restricted to this level so that binary
    // operators (in particular comparison) cannot nest inside them.
    _postfix_level: $ => choice(
      $.member_expression,
      $.call_expression,
      $.try_expression,
      $.primed_expression,
      $.parenthesized_expression,
      $.tuple_literal,
      $.vector_literal,
      $.block,
      $.if_expression,
      $.match_expression,
      $.set_form,
      $.bag_form,
      $.map_literal,
      $.record_literal,
      $.record_update,
      $.quantifier,
      $.option_literal,
      $.lambda,
      $.literal,
      $.ident,
    ),

    unary_expression: $ => prec.right(PREC.UNARY, seq(
      field('operator', choice('~', '-')),
      field('operand', $._unary_level),
    )),

    cast_expression: $ => prec.right(PREC.CAST, seq(
      field('operand', $._postfix_level),
      'as',
      field('type', $.type),
    )),

    // Field/tuple-index access. A method call `a.b(x)` parses as a
    // `call_expression` whose function is a `member_expression`.
    member_expression: $ => prec.left(PREC.POSTFIX, seq(
      field('operand', $._postfix_level),
      '.',
      field('member', choice($.ident, $.int_literal)),
    )),

    call_expression: $ => prec.left(PREC.POSTFIX, seq(
      field('function', $._postfix_level),
      optional(seq('::', field('type_arguments', $.type_arguments))),
      field('arguments', $.arguments),
    )),

    try_expression: $ => prec.left(PREC.POSTFIX, seq(
      field('operand', $._postfix_level),
      '?',
    )),

    // Next-state operator. Defined at postfix level (a superset of the
    // property-only usage in A.2, which keeps `total_balance()' = ...`
    // parseable inside parenthesized expressions).
    primed_expression: $ => prec.left(PREC.PRIME, seq(
      field('operand', $._postfix_level),
      "'",
    )),

    arguments: $ => seq('(', commaSep($.argument), ')'),

    argument: $ => seq(
      optional(seq(field('name', $.ident), ':')),
      field('value', $._expression),
    ),

    parenthesized_expression: $ => seq('(', $._expression, ')'),

    tuple_literal: $ => seq(
      '(',
      $._expression, ',',
      commaSep1($._expression),
      ')',
    ),

    vector_literal: $ => seq('[', commaSep($._expression), ']'),

    block: $ => seq(
      '{',
      repeat(seq('let', $.let_binding, ';')),
      field('body', $._expression),
      '}',
    ),

    let_binding: $ => seq(
      $.pattern,
      optional(seq(':', field('type', $.type))),
      '==',
      field('value', $._expression),
    ),

    if_expression: $ => prec.right(seq(
      'if',
      field('condition', $._expression),
      'then',
      field('consequence', $._expression),
      'else',
      field('alternative', $._expression),
    )),

    match_expression: $ => seq(
      'match',
      field('scrutinee', $._expression),
      '{',
      commaSep1($.match_arm),
      '}',
    ),

    match_arm: $ => seq(
      $.pattern,
      '=>',
      field('body', $._expression),
    ),

    // ------------------------------------------------------------------
    // Set / bag / map / record forms
    // ------------------------------------------------------------------

    set_form: $ => seq(
      'set', '{',
      choice(
        commaSep($._expression),
        $.filter_form,
        $.map_form,
      ),
      '}',
    ),

    bag_form: $ => seq(
      'bag', '{',
      choice(
        commaSep($._expression),
        $.map_form,
      ),
      '}',
    ),

    map_literal: $ => seq(
      'map', '{',
      commaSep($.map_entry),
      '}',
    ),

    map_entry: $ => seq(
      field('key', $._expression),
      ':',
      field('value', $._expression),
    ),

    filter_form: $ => seq(
      $.pattern,
      '\\in',
      field('collection', $._expression),
      'if',
      field('predicate', $._expression),
    ),

    map_form: $ => seq(
      field('key', $._expression),
      ':',
      commaSep1($.generator),
    ),

    generator: $ => seq(
      $.pattern,
      '\\in',
      field('collection', $._expression),
    ),

    record_literal: $ => seq(
      'record', '{',
      commaSep1($.record_field),
      '}',
    ),

    record_update: $ => seq(
      'record', '{',
      field('base', $._expression),
      'with',
      commaSep1($.record_field),
      '}',
    ),

    record_field: $ => seq(
      field('name', $.ident),
      ':',
      field('value', $._expression),
    ),

    quantifier: $ => seq(
      field('quantifier', choice('\\A', '\\E')),
      commaSep1($.generator),
      ':',
      field('body', $._expression),
    ),

    option_literal: $ => choice(
      seq('some', '(', field('value', $._expression), ')'),
      'none',
    ),

    // ------------------------------------------------------------------
    // Lambda
    // ------------------------------------------------------------------

    lambda: $ => seq(
      'lambda',
      optional($.capture_list),
      '(',
      commaSep($.lambda_parameter),
      ')',
      optional(seq('->', field('return_type', $.type))),
      field('body', $.block),
    ),

    capture_list: $ => seq('[', commaSep($.ident), ']'),

    lambda_parameter: $ => seq(
      $.pattern,
      optional(seq(':', field('type', $.type))),
    ),

    // ------------------------------------------------------------------
    // Patterns
    // ------------------------------------------------------------------

    pattern: $ => choice(
      $.wildcard_pattern,
      $.ident,
      $.int_literal,
      $.string_literal,
      $.boolean_literal,
      $.option_pattern,
      $.variant_pattern,
      $.tuple_pattern,
      $.record_pattern,
      $.vector_pattern,
    ),

    wildcard_pattern: $ => '_',

    option_pattern: $ => choice(
      'none',
      seq('some', '(', $.pattern, ')'),
    ),

    variant_pattern: $ => seq(
      field('name', $.ident),
      '(',
      commaSep1($.pattern),
      ')',
    ),

    tuple_pattern: $ => seq(
      '(',
      $.pattern, ',',
      commaSep1($.pattern),
      ')',
    ),

    record_pattern: $ => seq('{', commaSep1($.ident), '}'),

    vector_pattern: $ => choice(
      seq('[', ']'),
      seq('[', $.pattern, ',', '..', $.pattern, ']'),
    ),

    // ------------------------------------------------------------------
    // Literals
    // ------------------------------------------------------------------

    literal: $ => choice(
      $.int_literal,
      $.float_literal,
      $.string_literal,
      $.date_literal,
      $.decimal_literal,
      $.boolean_literal,
    ),

    boolean_literal: $ => choice('true', 'false'),

    int_literal: $ => token(choice(
      '0',
      /[1-9][0-9_]*/,
      /0x[0-9a-fA-F_]+/,
    )),

    float_literal: $ => token(choice(
      /[0-9][0-9_]*\.[0-9_]+([eE][+-]?[0-9]+)?/,
      /[0-9][0-9_]*[eE][+-]?[0-9]+/,
    )),

    string_literal: $ => seq(
      '"',
      repeat(choice(
        $.string_content,
        $.escape_sequence,
        $.interpolation,
      )),
      '"',
    ),

    string_content: $ => token(prec(1, /[^"\\]+/)),

    escape_sequence: $ => token(prec(2, choice(
      '\\n',
      '\\t',
      '\\\\',
      '\\"',
      /\\u\{[0-9a-fA-F]{1,6}\}/,
    ))),

    interpolation: $ => seq('\\(', $._expression, ')'),

    date_literal: $ => seq('date', field('value', $.string_literal)),

    decimal_literal: $ => seq(
      'decimal',
      optional(seq('(', $.int_literal, ',', $.int_literal, ')')),
      field('value', choice(
        $.int_literal,
        alias($._plain_float, $.float_literal),
      )),
    ),

    // Float without exponent, only used as the trailing part of a
    // decimal literal (A.1: `decimal(4,2) 3.14`).
    _plain_float: $ => token(/[0-9][0-9_]*\.[0-9_]+/),

    // ------------------------------------------------------------------
    // Lexical basics
    // ------------------------------------------------------------------

    ident: $ => /[a-zA-Z_][a-zA-Z0-9_]*/,

    comment: $ => token(choice(
      seq('//', /[^\n]*/),
      seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/'),
    )),
  },
});
