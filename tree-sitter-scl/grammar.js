/**
 * @file State Construction Language (SCL) is a database system language in which correctness is achieved by constructing only valid system states, rather than handling errors during execution.
 * @author scuptio
 * @license Apache
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// grammar.js
// SCL v1.0 — State Construction Language
// FINAL, merged, semantics-clean version

const PREC = {
    IMPLIES: 1,
    OR: 2,
    AND: 3,
    EQ: 4,
    REL: 5,
    ADD: 6,
    MUL: 7,
    UNARY: 8,
};

export default grammar({
    name: 'scl',

    extras: $ => [
        /\s/,
        $.comment,
    ],

    word: $ => $.identifier,

    rules: {

        /* =====================================================
         * Program
         * ===================================================== */

        program: $ =>
            repeat(
                choice(
                    $.type_decl,
                    $.context_decl,
                    $.state_decl
                )
            ),

        /* =====================================================
         * Composite Type
         * ===================================================== */

        type_decl: $ =>
            seq(
                'type',
                field('name', $.identifier),
                '{',
                repeat1($.type_field),
                '}'
            ),

        type_field: $ =>
            seq(
                field('name', $.identifier),
                ':',
                field('type', $.type),
                ';'
            ),

        /* =====================================================
         * Context
         * ===================================================== */

        context_decl: $ =>
            seq(
                'context',
                field('name', $.identifier),
                '{',
                repeat($.context_field),
                '}'
            ),

        context_field: $ =>
            seq(
                field('name', $.identifier),
                ':',
                field('type', $.type),
                ';'
            ),

        /* =====================================================
         * State
         * ===================================================== */

        state_decl: $ =>
            seq(
                'state',
                field('name', $.identifier),
                'uses',
                commaSep1($.identifier),
                '{',
                repeat($.use_stmt),
                optional($.precondition_block),
                repeat($.statement),
                $.next_block,
                '}'
            ),

        /* =====================================================
         * use — NAME INTRODUCTION ONLY
         * ===================================================== */

        use_stmt: $ =>
            choice(
                $.use_state_stmt,
                $.use_context_stmt,
                $.use_type_stmt
            ),

        use_state_stmt: $ =>
            seq(
                'use',
                'state',
                field('source', $.qualified_name),
                '{',
                repeat1($.use_field),
                '}'
            ),

        use_context_stmt: $ =>
            seq(
                'use',
                'context',
                field('context', $.qualified_name),
                ';'
            ),

        use_type_stmt: $ =>
            seq(
                'use',
                'type',
                field('type', $.qualified_name),
                ';'
            ),

        use_field: $ =>
            seq(
                field('name', $.identifier),
                ':',
                field('type', $.type),
                ';'
            ),

        /* =====================================================
         * Precondition
         * ===================================================== */

        precondition_block: $ =>
            seq(
                choice('precondition', 'pre'),
                '{',
                repeat1(seq($.condition, ';')),
                '}'
            ),

        /* =====================================================
         * Statements
         * ===================================================== */

        statement: $ =>
            choice(
                $.let_stmt,
                $.select_stmt,
                $.foreach_stmt,
                $.update_stmt,
                $.insert_stmt,
                $.delete_stmt,
                $.assert_stmt,
                $.commit_stmt
            ),

        let_stmt: $ =>
            seq(
                'let',
                field('name', $.identifier),
                ':',
                field('type', $.type),
                '=',
                field('value', $.expr),
                ';'
            ),

        select_stmt: $ =>
            seq(
                'select',
                field('name', $.identifier),
                ':',
                field('type', $.type),
                'from',
                field('entity', $.identifier),
                optional($.where_clause),
                optional($.limit_clause),
                ';'
            ),

        foreach_stmt: $ =>
            seq(
                'foreach',
                field('item', $.identifier),
                ':',
                field('item_type', $.type),
                'in',
                field('collection', $.identifier),
                '{',
                repeat($.statement),
                '}'
            ),

        update_stmt: $ =>
            seq(
                'update',
                field('entity', $.identifier),
                'set',
                commaSep1($.assignment),
                optional($.where_clause),
                ';'
            ),

        assignment: $ =>
            seq(
                field('field', $.identifier),
                '=',
                field('value', $.expr)
            ),

        insert_stmt: $ =>
            seq(
                'insert',
                'into',
                field('entity', $.identifier),
                '(',
                commaSep($.identifier),
                ')',
                'values',
                '(',
                commaSep($.expr),
                ')',
                ';'
            ),

        delete_stmt: $ =>
            seq(
                'delete',
                'from',
                field('entity', $.identifier),
                optional($.where_clause),
                ';'
            ),

        where_clause: $ =>
            seq('where', $.expr),

        limit_clause: $ =>
            seq('limit', $.number),

        assert_stmt: $ =>
            seq('assert', $.expr, ';'),

        commit_stmt: $ =>
            seq('commit', ';'),

        /* =====================================================
         * Next — THE ONLY CONTROL FLOW
         * ===================================================== */

        next_block: $ =>
            seq(
                'next',
                '{',
                repeat1($.next_case),
                '}'
            ),

        next_case: $ =>
            choice(
                seq(
                    'when',
                    $.condition,
                    '=>',
                    field('target', $.identifier),
                    optional($.edge_export_block)
                ),
                seq(
                    'otherwise',
                    '=>',
                    field('target', $.identifier),
                    optional($.edge_export_block)
                )
            ),

        edge_export_block: $ =>
            seq(
                '{',
                'export',
                repeat1($.edge_field),
                '}'
            ),

        edge_field: $ =>
            seq(
                field('name', $.identifier),
                ':',
                field('type', $.type),
                ';'
            ),

        /* =====================================================
         * Condition — TLA+ SUBSET
         * ===================================================== */

        condition: $ => $.tla_expr,

        tla_expr: $ =>
            choice(
                $.tla_binary_expr,
                $.tla_unary_expr,
                $.tla_quantifier,
                $.expr
            ),

        tla_binary_expr: $ =>
            choice(
                prec.left(PREC.IMPLIES, seq($.tla_expr, '=>', $.tla_expr)),
                prec.left(PREC.OR, seq($.tla_expr, '\\/', $.tla_expr)),
                prec.left(PREC.AND, seq($.tla_expr, '/\\', $.tla_expr))
            ),

        tla_unary_expr: $ =>
            prec(PREC.UNARY, seq('~', $.tla_expr)),

        tla_quantifier: $ =>
            seq(
                choice('\\E', '\\A'),
                'Relation',
                field('relation', $.identifier),
                field('var', $.identifier),
                'by',
                $.expr,           // PK binding (checked semantically)
                ':',
                $.tla_expr
            ),

        /* =====================================================
         * Expressions
         * ===================================================== */

        expr: $ =>
            choice(
                $.binary_expr,
                $.unary_expr,
                $.field_access,
                $.struct_literal,
                $.identifier,
                $.literal,
                seq('(', $.expr, ')')
            ),

        struct_literal: $ =>
            seq(
                '{',
                commaSep1($.struct_field),
                '}'
            ),

        struct_field: $ =>
            seq(
                field('name', $.identifier),
                ':',
                field('value', $.expr)
            ),

        binary_expr: $ =>
            choice(
                prec.left(PREC.EQ, seq($.expr, choice('=', '#', '==', '!='), $.expr)),
                prec.left(PREC.REL, seq($.expr, choice('<', '<=', '>', '>='), $.expr)),
                prec.left(PREC.ADD, seq($.expr, choice('+', '-'), $.expr)),
                prec.left(PREC.MUL, seq($.expr, choice('*', '/'), $.expr))
            ),

        unary_expr: $ =>
            prec(PREC.UNARY, seq(choice('!', '-'), $.expr)),

        field_access: $ =>
            seq(
                field('object', $.identifier),
                '.',
                field('field', $.identifier)
            ),

        /* =====================================================
         * Types
         * ===================================================== */

        type: $ =>
            choice(
                $.primitive_type,
                $.generic_type,
                $.identifier
            ),

        primitive_type: _ =>
            choice('int', 'bool', 'string', 'float'),

        generic_type: $ =>
            seq(
                field('base', $.identifier),
                '[',
                field('param', $.type),
                ']'
            ),

        /* =====================================================
         * Names / Literals
         * ===================================================== */

        qualified_name: $ =>
            choice(
                $.identifier,
                seq($.string, '::', $.identifier)
            ),

        literal: $ =>
            choice(
                $.number,
                $.string,
                $.boolean,
                'null'
            ),

        number: _ => /\d+/,

        boolean: _ => choice('true', 'false'),

        string: _ =>
            seq('"', repeat(choice(/[^"\\]/, /\\./)), '"'),

        identifier: _ =>
            /[a-zA-Z_][a-zA-Z0-9_]*/,

        comment: _ =>
            token(
                choice(
                    seq('//', /.*/),
                    seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/')
                )
            ),
    }
});

/* =====================================================
 * Helpers
 * ===================================================== */

function commaSep(rule) {
    return optional(commaSep1(rule));
}

function commaSep1(rule) {
    return seq(rule, repeat(seq(',', rule)));
}

