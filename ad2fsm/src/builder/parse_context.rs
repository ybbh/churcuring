use crate::builder::parse_error::{node_to_sexpr_with_text, SexpOptions};
use anyhow::Result;
use std::fmt::Write;
use tree_sitter::Node;

pub struct ParseContext {
    text: String,
}

impl ParseContext {
    pub fn new(text: String) -> ParseContext {
        Self { text }
    }

    pub fn text_of_node(&self, node: &Node) -> Result<String> {
        let text = node.utf8_text(self.text.as_bytes())?;
        Ok(text.to_string())
    }


    pub fn print_error_line<W: Write>(&self, node: Node, writter: &mut W) -> Result<()> {
        // row and column start at 0
        let line_start = node.start_position().row + 1;
        let line_end = node.end_position().row + 1;
        let column_start = node.start_position().column + 1;
        let column_end = node.end_position().column + 1;

        let mut cursor = node.walk();
        let mut tokens = String::new();

        for (i, child) in node.children(&mut cursor).enumerate() {
            let text = self.text_of_node(&child)?;
            if i != 0 {
                tokens.push_str(", ");
            }
            tokens.push_str(&text);
        }
        let kind = if let Some(parent) = node.parent() {
            parent.kind()
        } else {
            "root"
        };
        let error_text = error_text(&self.text, line_start, column_start, line_end, column_end)?;

        let options = SexpOptions {
            show_text: true,
            show_byte_range: false,
            show_position: false,
            max_text_length: Some(20),
        };
        let sexp_text = node_to_sexpr_with_text(&node, &self.text, &options);
        let error_msg = format!(
            "In \
        position: [{},{}; {},{}]\n, \
        text: [{}]\n
        child tokens:[{}]\n, \
        parent kind:[{}]\n,\
        s-expr: [{}]\n",
            line_start,
            column_start,
            line_end,
            column_end,
            error_text,
            tokens,
            kind,
            sexp_text
        );

        writter.write_fmt(format_args!("{}", error_msg)).unwrap();
        Ok(())
    }
}


fn error_text(
    parse_text: &str,
    line_start: usize,
    column_start: usize,
    line_end: usize,
    column_end: usize,
) -> Result<String> {
    let line_start = line_start - 1;
    let column_start = column_start - 1;
    let line_end = line_end - 1;
    let column_end = column_end - 1;

    let mut err_text = String::new();
    let lines: Vec<_> = parse_text.lines().collect();
    for i in line_start..=line_end {
        let opt = lines.get(i);
        if let Some(s) = opt {
            let str = if i == line_start && i != line_end {
                s[column_start..].to_string()
            } else if i != line_end && i == line_end {
                s[..column_end].to_string()
            } else if i == line_start && i == line_end {
                s[column_start..column_end].to_string()
            } else {
                s.to_string()
            };
            err_text.push_str(&str);
        } else {
            err_text.clear();
            break;
        }
    }
    Ok(err_text)
}
