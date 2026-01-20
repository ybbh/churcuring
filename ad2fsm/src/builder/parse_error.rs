use tree_sitter::Node;

pub struct SexpOptions {
    pub show_text: bool,
    pub show_byte_range: bool,
    pub show_position: bool,
    pub max_text_length: Option<usize>,
}

impl Default for SexpOptions {
    fn default() -> Self {
        Self {
            show_text: true,
            show_byte_range: false,
            show_position: false,
            max_text_length: Some(100),
        }
    }
}

pub fn node_to_sexpr_with_text(
    node: &Node,
    source: &str,
    options: &SexpOptions,
) -> String {
    node_to_sexpr_recursive(node, source, options, 0)
}

fn node_to_sexpr_recursive(
    node: &Node,
    source: &str,
    options: &SexpOptions,
    depth: usize,
) -> String {
    let mut parts = Vec::new();

    // node type
    parts.push(node.kind().to_string());

    // additional meta data
    let mut metadata = Vec::new();

    if options.show_text {
        if let Ok(text) = node.utf8_text(source.as_bytes()) {
            let text = if let Some(max_len) = options.max_text_length {
                if text.len() > max_len {
                    format!("{}...", &text[..max_len])
                } else {
                    text.to_string()
                }
            } else {
                text.to_string()
            };

            if !text.is_empty() {
                metadata.push(format!("text=\"{}\"", escape_string(&text)));
            }
        }
    }

    if options.show_byte_range {
        metadata.push(format!("bytes={}..{}", node.start_byte(), node.end_byte()));
    }

    if options.show_position {
        let start = node.start_position();
        let end = node.end_position();
        metadata.push(format!("pos=({},{})-({},{})",
                              start.row, start.column,
                              end.row, end.column
        ));
    }

    // if exist metadata
    if !metadata.is_empty() {
        parts.insert(0, format!("[{}]", metadata.join(" ")));
    }

    // handle child node
    let mut cursor = node.walk();
    let mut has_children = false;
    let mut children_parts = Vec::new();

    for child in node.children(&mut cursor) {
        has_children = true;
        children_parts.push(node_to_sexpr_recursive(&child, source, options, depth + 1));
    }

    // the result
    if has_children {
        if children_parts.len() == 1 {
            format!("({} {})", parts.join(":"), children_parts[0])
        } else {
            format!("({}\n{} {})",
                    parts.join(":"),
                    "  ".repeat(depth + 1),
                    children_parts.join(&format!("\n{}", "  ".repeat(depth + 1)))
            )
        }
    } else {
        format!("({})", parts.join(":"))
    }
}


fn escape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 2);

    for c in s.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\x08' => result.push_str("\\b"),
            '\x0C' => result.push_str("\\f"),
            _ if c.is_control() => result.push_str(&format!("\\u{:04x}", c as u32)),
            _ => result.push(c),
        }
    }

    result
}