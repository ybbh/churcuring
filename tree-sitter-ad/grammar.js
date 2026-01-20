/**
 * @file PlantUML Activity Diagram Parser
 * @description Tree-sitter grammar for PlantUML Activity Diagram (new syntax)
 * @license Apache License 2.0
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const commaSeparatedList = (rule) =>
    seq(rule, repeat(seq(',', rule)), optional(','));

export default grammar({
    name: "activity_diagram",


    // Main grammar rules
    rules: {
        // ===================================================================
        // DOCUMENT STRUCTURE
        // ===================================================================

        // Root rule: entire PlantUML file
        document: $ => seq(
            optional($.startuml_directive),   // @startuml optional
            optional('start'),
            repeat1($._top_statement),
            optional('end'),
            optional($.enduml_directive)      // @enduml optional
        ),

        _top_statement: $ => choice(
            $.define_statement,
            $.if_statement,
            $.switch_statement,
            $.while_statement,
            $.repeat_statement,
            $.group_statement,
            $.terminal_statement
        ),

        // PlantUML start directive
        startuml_directive: $ => prec.right(choice(
            seq("@startuml", $.text_content),
            "@startuml"
        )),

        // PlantUML end directive
        enduml_directive: $ => "@enduml",

        define_statement: $ => choice(
            // Styling and metadata
            $.title_statement,                // title
            $.skinparam,                      // skinparam
            $.style_block,                    // <style> blocks
            $.pragma,                         // !pragma directives
        ),

        // ===================================================================
        // STATEMENTS: All valid diagram elements
        // ===================================================================
        terminal_statement: $ => choice(
            $.stop,
            $.break_statement,                // break
            $.goto_statement,                 // goto and label

            $.note_statement,                 // notes
            $.arrow,                          // arrows

            // Action statements
            $.action_statement,               // action;
        ),

        stop: $ => prec.right(2, seq('stop', optional(';'))),

        // ===================================================================
        // CONDITIONAL STATEMENTS
        // ===================================================================

        // If statement with various syntaxes

        if_statement: $ => seq(
            $.if_condition,
            repeat($.elseif_condition),
            optional($.else_condition),
            $.endif_directive
        ),

        endif_directive: $ => choice(
            'endif',
            seq('end', 'if')
        ),


        if_condition: $ => seq(
            // Standard if
            'if',
            optional('('), field('expression', $.expression), optional(')'),
            optional('then'),
            optional(seq('(', $.text_content, ')')),  // Optional label in parentheses
            field('block_statement_list', $.block_statement_list)
        ),

        elseif_condition: $ => seq(
            'elseif',
            optional('('), field('expression', $.expression), optional(')'),
            optional('then'),
            optional(seq('(', $.text_content, ')')),  // Optional label in parentheses
            field('block_statement_list', $.block_statement_list)
        ),

        else_condition: $ => seq(
            'else',
            optional(seq('(', $.text_content, ')')),  // Optional label in parentheses
            field('block_statement_list', $.block_statement_list)
        ),


        block_statement_list: $ => repeat1($._block_statement),

        _block_statement: $ => choice(
            $.if_statement,
            $.switch_statement,
            $.while_statement,
            $.repeat_statement,
            $.group_statement,
            $.terminal_statement
        ),


        // Switch statement
        switch_statement: $ => seq(
            'switch',
            optional('('),
            field('expression', $.expression),
            optional(')'),
            optional(';'),
            repeat1($.case_clause),            // One or more case clauses
            $.endswitch_directive
        ),

        endswitch_directive: $ => choice(
            'endswitch',
            seq('end', 'switch')
        ),

        // Case clause in switch statement
        case_clause: $ => seq(
            'case',
            optional('('),
            field('expression', $.expression),
            optional(')'),
            field('block_statement_list', $.block_statement_list)
        ),

        // ===================================================================
        // LOOPS
        // ===================================================================

        // Repeat loop
        repeat_statement: $ => seq(
            'repeat',
            field('block_statement_list', $.block_statement_list),
            $.repeat_statement_end
        ),

        repeat_statement_end: $ =>
            seq(
                'repeatwhile',
                optional('('),
                field('expression', $.expression),
                optional(')')
            ),


        // While loop
        while_statement: $ => seq(
            'while',
            optional('('),
            field('expression', $.expression),
            optional(')'),
            optional(seq(optional('is'), '(', $.text_content, ')')),  // Optional label in parentheses
            field('block_statement_list', $.block_statement_list),
            $.endwhile_directive,
            optional(seq('(', $.text_content, ')'))  // Optional label in parentheses
        ),

        endwhile_directive: $ => choice(
            'endwhile',
            seq('end', 'while')
        ),

        // Break statement
        break_statement: $ => 'break',

        // Goto and label statements
        goto_statement: $ => prec.right(3, seq(
            choice(
                seq('label', $.identifier),      // Define label
                seq('goto', $.identifier)        // Jump to label
            ),
            optional(';')
        )),

        // ===================================================================
        // GROUPING AND ORGANIZATION
        // ===================================================================

        group_type: $ => choice(
            'group',
            'partition',
            'package',
            'rectangle',
            'card'
        ),

        // Group/partition/package/rectangle/card statements
        group_statement: $ => prec.right(choice(
            seq(
                field('type', $.group_type),
                optional(field('name', $.text_content)),  // Group name
                optional(field('color', $.color_value)),  // Optional color
                field('block_statement_list', $.block_statement_list),
                seq('end', field('type', $.group_type))
            ),
            seq(
                field('type', $.group_type),
                optional(field('name', $.text_content)),  // Group name
                optional(field('color', $.color_value)),  // Optional color
                '{',
                field('block_statement_list', $.block_statement_list),
                '}'
            )
        )),

        // Note statement
        note_statement: $ => seq(
            optional('floating'),
            'note',
            optional(field('position', choice('left', 'right', 'top', 'bottom'))),
            optional(':'),
            field('content', $.text_content),
            choice('endnote', seq('end', 'note'))
        ),

        // ===================================================================
        // CONNECTORS AND ARROWS
        // ===================================================================

        // Arrow with optional styling
        arrow: $ => seq(
            field('arrow', $.arrow_style),              // Arrow style
            $.action_statement
        ),

        // Arrow style definition
        arrow_style: $ => choice(
            '->',                             // Simple arrow
            '-->',                            // Dotted arrow
            seq('-', '[', $.arrow_properties, ']', '->'),  // Styled arrow
            seq('-', '[', 'hidden', ']', '->'),            // Hidden arrow
        ),

        // Arrow properties (color, style, etc.)
        arrow_properties: $ => commaSeparatedList($.arrow_property_element),

        arrow_property_element: $ => choice($.color_value, 'bold', 'dashed', 'dotted'),

        // ===================================================================
        // STYLING AND METADATA
        // ===================================================================

        // Title statement
        title_statement: $ => seq(
            'title',
            field('text', $.text_content),
            optional(';')
        ),

        // Skinparam for styling
        skinparam: $ => seq(
            'skinparam',
            field('element', $.identifier),
            field('property', $.identifier),
            field('value', $.skinparam_value),
            optional(';')
        ),

        // Skinparam value types
        skinparam_value: $ => choice(
            $.color_value,
            $.text_content,
            $.identifier
        ),

        // Style block for CSS-like styling
        style_block: $ => seq(
            '<style>',
            repeat($.style_rule),
            '</style>'
        ),

        // Style rule within style block
        style_rule: $ => seq(
            field('selector', $.identifier),
            '{',
            repeat($.style_property),
            '}'
        ),

        // Style property
        style_property: $ => seq(
            field('property', $.identifier),
            ':',
            field('value', $.text_content),
            ';'
        ),

        // Pragma directive
        pragma: $ => seq(
            '!pragma',
            field('name', $.identifier),
            optional(seq(field('operator', choice('=', 'on', 'off')), field('value', $.text_content)))
        ),

        // ===================================================================
        // SDL AND UML SHAPES (Stereotypes)
        // ===================================================================

        // SDL/UML shape with stereotype
        sdl_shape: $ => seq(
            $.text_content,
            ';',
            optional(seq('<<', $.stereotype, '>>'))
        ),

        // Stereotype definitions
        stereotype: $ => choice(
            'input',
            'output',
            'procedure',
            'load',
            'save',
            'continuous',
            'task',
            'object',
            'objectSignal',
            'object-signal',
            'acceptEvent',
            'accept-event',
            'timeEvent',
            'time-event',
            'sendSignal',
            'send-signal',
            'trigger',
            'icon'  // For emoji actions
        ),

        // ===================================================================
        // EXPRESSIONS AND VALUES
        // ===================================================================

        // Expression for conditions
        expression: $ => choice(
            seq("activity", field('activity_identifier', $.identifier)),
            field('expression_content', $.text_content,),
        ),

        // Color expression for conditions
        color_expression: $ => seq(
            '<color:',
            $.color_value,
            '>',
            $.identifier
        ),

        // ===================================================================
        // LEXICAL TOKENS
        // ===================================================================

        // Text content
        text_content: $ => repeat1($.text_word),

        text_word: $ => prec(-1, token(/[^ \t\n\r,;{}()]+/)),

        // Action statement (for statement statements)
        action_statement: $ => seq(
            ':',
            field('action', $.text_content),
            ';'
        ),

        // Identifier (variable names, labels, etc.)
        identifier: $ => /[a-zA-Z_][a-zA-Z0-9_]*/,

        // Color values (hex, named colors, or gradients)
        color_value: $ => choice(
            seq($.color_value_item, '/', $.color_value_item),  // Gradients
            seq($.color_value_item, '\\', $.color_value_item),  // Backslash gradients
            $.color_value_item
        ),

        color_value_item: $ => choice(
            /#[0-9A-Fa-f]{3,6}/,                     // Hex colors
            /[a-zA-Z]+/,                             // Named colors
        ),

        // Number values
        number: $ => /[0-9]+(\.[0-9]+)?/,

        // Boolean values
        boolean: $ => choice('true', 'false'),

        // Emoji or special characters (for icon stereotypes)
        emoji: $ => /:\w+:/,


    }
});