use crate::builder::ast_kind::{ASTKind, CondBody, IfElseBlock};
use crate::builder::parse_context::ParseContext;
use crate::ts_const;
use anyhow::{Error, Result};
use regex::Regex;
use tree_sitter::{Node, Parser};
use tree_sitter_ad::LANGUAGE;

/// Main parser for PlantUML activity diagrams
///
/// This parser uses Tree-sitter to parse PlantUML syntax and build
/// an Abstract Syntax Tree (AST) for further processing.
pub struct ADParser {
    parser: Parser,
}

impl ADParser {
    /// Creates a new parser instance with the PlantUML grammar
    pub fn new() -> Self {
        let mut parser = Parser::new();

        parser.set_language(&LANGUAGE.into())
            .expect("Failed to set language");

        ADParser { parser }
    }

    /// Parses PlantUML source code and builds an AST
    ///
    /// # Arguments
    /// * `source_code` - The PlantUML source code to parse
    ///
    /// # Returns
    /// * `Result<ParseContext>` - return parse context
    pub fn parse(&mut self, source_code: &str) -> Result<Vec<ASTKind>> {
        // Parse the source code into a Tree-sitter tree
        let source_pre_processed = Self::pre_process(source_code);
        let opt_tree = self.parser
            .parse(source_pre_processed, None);

        let tree = opt_tree.expect("Failed to parse source code");

        let root_node = tree.root_node();
        let mut context = ParseContext::new(source_code.to_string());

        // Build our custom AST from the Tree-sitter tree
        let ast_list = self.traverse_node(root_node, &mut context)?;
        Ok(ast_list)
    }

    fn pre_process(text: &str) -> String {
        Self::preprocess_repeat_while(text)
    }

    /// Normalize `repeat while (...)` into `repeatwhile (...)`
    pub fn preprocess_repeat_while(input: &str) -> String {
        //  repeat while -> repeatwhile
        let re = Regex::new(r"(?m)^(\s*)repeat\s+while\b").unwrap();

        re.replace_all(input, |caps: &regex::Captures| {
            format!("{}repeatwhile", &caps[1])
        })
            .to_string()
    }

    /// Recursively traverse a Node from Tree-sitter nodes
    ///
    /// This function traverses the Tree-sitter parse tree and creates
    /// a simplified Node with extracted the content.
    fn traverse_node(&self, node: Node, context: &mut ParseContext) -> Result<Vec<ASTKind>> {
        let node_type = node.kind();
        let mut vec_ast = vec![];
        if node.has_error() {
            let mut str = String::new();
            context.print_error_line(node, &mut str)?;
            println!("{}", str);
        }
        match node_type {
            ts_const::ts_kind_name::S_IF_STATEMENT => {
                let ast = self.visit_if_statement(node, context)?;
                vec_ast.push(ast)
            }
            ts_const::ts_kind_name::S_WHILE_STATEMENT => {
                let ast = self.visit_while_statement(node, context)?;
                vec_ast.push(ast)
            }
            ts_const::ts_kind_name::S_ACTION_STATEMENT => {
                let ast = self.visit_action_statement(node, context)?;
                vec_ast.push(ast)
            }
            _ => {
                // Recursively process child nodes
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    let vec = self.traverse_node(child, context)?;
                    vec_ast.extend(vec);
                }
            }
        }

        Ok(vec_ast)
    }

    fn get_named_field<'a>(&self, node: &Node<'a>, field_name: &str) -> Result<Node<'a>> {
        let child = node.child_by_field_name(field_name)
            .map_or_else(
                || { Err(Error::msg(format!("expected get field {}", field_name))) },
                |c| { Ok(c) })?;
        Ok(child)
    }

    fn visit_if_statement(&self, node: Node, context: &mut ParseContext) -> Result<ASTKind> {
        let mut cursor = node.walk();
        let mut block = IfElseBlock {
            if_elif: vec![],
            else_: vec![],
        };
        for child in node.children(&mut cursor) {
            let child_kind = child.kind();
            if child_kind == ts_const::ts_kind_name::S_IF_CONDITION {
                let cond_body = self.visit_if_condition(child, context)?;
                block.if_elif.push(cond_body);
            } else if child_kind == ts_const::ts_kind_name::S_ELSEIF_CONDITION {
                let cond_body = self.visit_if_condition(child, context)?;
                block.if_elif.push(cond_body);
            } else if child_kind == ts_const::ts_kind_name::S_ELSE_CONDITION {
                let else_ = self.visit_else_condition(child, context)?;
                block.else_.extend(else_);
            }
        }
        Ok(ASTKind::IfElse(block))
    }

    fn visit_if_condition(&self, node: Node, context: &mut ParseContext) -> Result<CondBody> {
        let expression = self.get_named_field(&node, ts_const::ts_field_name::EXPRESSION)?;
        let cond = self.visit_expression(expression, context)?;
        let node_body = self.get_named_field(&node, ts_const::ts_kind_name::S_BLOCK_STATEMENT_LIST)?;
        let body = self.visit_block_statement_list(node_body, context)?;
        Ok(CondBody {
            cond,
            body,
        })
    }

    fn visit_else_condition(&self, node: Node, context: &mut ParseContext) -> Result<Vec<ASTKind>> {
        let node_body = self.get_named_field(&node, ts_const::ts_kind_name::S_BLOCK_STATEMENT_LIST)?;
        let body = self.visit_block_statement_list(node_body, context)?;
        Ok(body)
    }

    fn visit_expression(&self, node: Node, context: &mut ParseContext) -> Result<String> {
        let content = context.text_of_node(&node)?;
        Ok(content)
    }

    fn visit_block_statement_list(&self, node: Node, context: &mut ParseContext) -> Result<Vec<ASTKind>> {
        let mut ret = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let vec_ast_kind = self.traverse_node(child, context)?;
            ret.extend(vec_ast_kind);
        }
        Ok(ret)
    }

    fn visit_while_statement(&self, node: Node, context: &mut ParseContext) -> Result<ASTKind> {
        let node_expr = self.get_named_field(&node, ts_const::ts_field_name::EXPRESSION)?;
        let cond = self.visit_expression(node_expr, context)?;
        let node_body = self.get_named_field(&node, ts_const::ts_kind_name::S_BLOCK_STATEMENT_LIST)?;
        let body = self.visit_block_statement_list(node_body, context)?;
        Ok(ASTKind::While(CondBody {
            cond,
            body,
        }))
    }

    fn visit_action_statement(&self, node: Node, context: &mut ParseContext) -> Result<ASTKind> {
        let simple = self.get_named_field(&node, ts_const::ts_field_name::ACTION)?;
        let content = context.text_of_node(&simple)?;
        Ok(ASTKind::SimpleStmt(content))
    }
}


pub fn parse_with_tree_sitter(source: &str) -> Result<Vec<ASTKind>> {
    let mut parser = ADParser::new();
    let context = parser.parse(source)?;
    Ok(context)
}

#[cfg(test)]
mod tests {
    use crate::builder::parser::ADParser;
    use anyhow::Result;
    #[test]
    fn test_parser() -> Result<()> {
        let text = include_str!("test_data/activity.puml");
        let mut parser = ADParser::new();
        let _context = parser.parse(text)?;
        Ok(())
    }
}