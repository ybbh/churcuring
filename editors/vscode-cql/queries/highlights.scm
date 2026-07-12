; CQL syntax highlighting queries (tree-sitter).
;
; Node/field names follow crates/tree-sitter-cql/grammar.js. Anonymous
; keyword tokens are captured either globally (reserved words, which can
; only lex as keywords where the grammar expects them) or scoped to the
; node in which they appear (contextual keywords that are also valid
; identifiers elsewhere).

; ------------------------------------------------------------------
; Reserved keywords
; ------------------------------------------------------------------

[
  "module"
  "use"
  "const"
  "type"
  "enum"
  "index"
  "function"
  "recursive"
  "query"
  "action"
  "invariant"
  "test"
  "fixture"
  "expect"
  "property"
  "fairness"
  "let"
  "if"
  "then"
  "else"
  "match"
  "some"
  "none"
  "true"
  "false"
  "as"
  "on"
  "lambda"
  "weak"
  "strong"
] @keyword

(visibility) @keyword

; `table` is a declaration keyword but also a type constructor and a valid
; identifier, so capture it where it appears.
(table_declaration "table" @keyword)

; Collection/record form keywords (these words double as builtin type
; constructors; in expression position they are keywords).
(set_form "set" @keyword)
(bag_form "bag" @keyword)
(map_literal "map" @keyword)
(record_literal "record" @keyword)
(record_update ["record" "with"] @keyword)

; ------------------------------------------------------------------
; Contextual keywords (scoped to the node where they appear)
; ------------------------------------------------------------------

(primary_key_clause ["primary" "key"] @keyword)
(foreign_key_clause ["foreign" "key" "references"] @keyword)
(key_type "key" @keyword)
(value_type "value" @keyword)
(decreases_clause "decreases" @keyword)
(depth_clause ["with" "depth"] @keyword)
(until_expression "until" @keyword)
(use_declaration "as" @keyword)

; ------------------------------------------------------------------
; Builtin / predeclared types
; ------------------------------------------------------------------

(primitive_type) @type.builtin
(decimal_type "decimal" @type.builtin)
(option_type "option" @type.builtin)
(vector_type "vector" @type.builtin)
(set_type "set" @type.builtin)
(bag_type "bag" @type.builtin)
(map_type "map" @type.builtin)
(table_type "table" @type.builtin)

; `set<write_op>` return type of actions.
(action_declaration ["set" "write_op"] @type.builtin)

; Literal prefixes: `date "2026-01-01"`, `decimal(4,2) 3.14`.
(date_literal "date" @type.builtin)
(decimal_literal "decimal" @type.builtin)

; ------------------------------------------------------------------
; Declarations and names
; ------------------------------------------------------------------

(source_file name: (ident) @namespace)
(use_declaration (ident) @namespace)
(use_declaration alias: (ident) @namespace)

(function_declaration name: (ident) @function)
(query_declaration name: (ident) @function)
(action_declaration name: (ident) @function)

(call_expression function: (ident) @function)

(type_declaration name: (ident) @type)
(enum_declaration name: (ident) @type)
(named_type name: (ident) @type)
(table_declaration name: (ident) @type)
(foreign_key_clause target: (ident) @type)
(index_declaration table: (ident) @type)
(invariant_declaration table: (ident) @type)
(key_type table: (ident) @type)
(value_type table: (ident) @type)

(type_parameters (ident) @type.parameter)

(variant name: (ident) @enumMember)
(variant_pattern name: (ident) @enumMember)

(const_declaration name: (ident) @constant)
(invariant_declaration name: (ident) @constant)
(property_declaration name: (ident) @constant)

(index_declaration name: (ident) @variable)
(test_declaration name: (ident) @variable)
(fixture_statement name: (ident) @variable)

(parameter name: (ident) @variable.parameter)
(lambda_parameter (pattern (ident) @variable.parameter))
(capture_list (ident) @variable)

; Bindings introduced by patterns.
(let_binding (pattern (ident) @variable))
(generator (pattern (ident) @variable))
(filter_form (pattern (ident) @variable))
(record_pattern (ident) @variable)
(wildcard_pattern) @variable.builtin

; Record / field access.
(field_declaration name: (ident) @property)
(record_field name: (ident) @property)
(member_expression member: (ident) @property)
(argument name: (ident) @property)

; Fairness target declarations.
(fairness_declaration (ident) @function)

; Any remaining identifier.
(ident) @variable

; ------------------------------------------------------------------
; Operators
; ------------------------------------------------------------------

(binary_expression operator: _ @operator)
(comparison_expression operator: _ @operator)
(additive_expression operator: _ @operator)
(multiplicative_expression operator: _ @operator)
(unary_expression operator: _ @operator)
(quantifier quantifier: _ @operator)

(member_expression "." @operator)
(call_expression "::" @operator)
(try_expression "?" @operator)
(primed_expression "'" @operator)
(match_arm "=>" @operator)
(vector_pattern ".." @operator)
(leads_to_expression "~>" @operator)
(eventually_expression "<>" @operator)
(always_expression ["[" "]"] @operator)
(use_declaration "::" @operator)

[
  "->"
  "=="
] @operator

; ------------------------------------------------------------------
; Literals
; ------------------------------------------------------------------

(int_literal) @number
(float_literal) @number

(string_literal "\"" @string)
(string_content) @string
(escape_sequence) @string.escape

; Interpolation delimiters `\(` ... `)`; the inner expression keeps its
; normal highlighting.
(interpolation ["\\(" ")"] @punctuation.special)

; ------------------------------------------------------------------
; Comments & punctuation
; ------------------------------------------------------------------

(comment) @comment

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

; `<`/`>` double as comparison operators, so only capture them as brackets
; in generic-type positions.
(option_type ["<" ">"] @punctuation.bracket)
(vector_type ["<" ">"] @punctuation.bracket)
(set_type ["<" ">"] @punctuation.bracket)
(bag_type ["<" ">"] @punctuation.bracket)
(map_type ["<" ">"] @punctuation.bracket)
(table_type ["<" ">"] @punctuation.bracket)
(type_arguments ["<" ">"] @punctuation.bracket)
(type_parameters ["<" ">"] @punctuation.bracket)
(action_declaration ["<" ">"] @punctuation.bracket)

[
  ","
  ";"
  ":"
] @punctuation.delimiter
