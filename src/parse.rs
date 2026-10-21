//! Parse an Apache .htaccess file into directives and blocks.
//!
//! Forgiving by design: an unknown directive is kept so the compiler can skip
//! it, and only structural errors (an unclosed block) reject the file.

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Directive { name: String, args: Vec<String>, line: usize },
    Block { name: String, args: Vec<String>, line: usize, children: Vec<Node> },
}

impl Node {
    pub fn key(&self) -> String {
        match self {
            Node::Directive { name, .. } | Node::Block { name, .. } => name.to_ascii_lowercase(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct ParseError(pub String);

/// Split like Apache: whitespace separates, double or single quotes group, backslash escapes.
pub fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (_, '\\') if chars.peek().is_some() => {
                let next = *chars.peek().unwrap();
                // Only a quote or a backslash is unescaped; regex escapes such as \. are kept.
                match (quote, next) {
                    (Some(q), n) if n == q => {
                        chars.next();
                        current.push(n);
                    }
                    (_, '\\') => {
                        chars.next();
                        current.push('\\');
                    }
                    (None, n) if n.is_whitespace() => {
                        chars.next();
                        current.push(n);
                    }
                    _ => current.push('\\'),
                }
                started = true;
            }
            (Some(q), c) if c == q => quote = None,
            (None, '"') | (None, '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (_, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(current);
    }
    out
}

/// Lines with comments removed and backslash continuations joined: (line number, text).
fn logical_lines(source: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut buffer = String::new();
    let mut start = 0;
    for (index, raw) in source.lines().enumerate() {
        let line = raw.trim();
        if buffer.is_empty() {
            start = index + 1;
        }
        if let Some(stripped) = line.strip_suffix('\\') {
            buffer.push_str(stripped);
            buffer.push(' ');
            continue;
        }
        buffer.push_str(line);
        let joined = std::mem::take(&mut buffer);
        let joined = joined.trim();
        if !joined.is_empty() && !joined.starts_with('#') {
            lines.push((start, joined.to_string()));
        }
    }
    let rest = buffer.trim();
    if !rest.is_empty() && !rest.starts_with('#') {
        lines.push((start, rest.to_string()));
    }
    lines
}

pub fn parse(source: &str) -> Result<Vec<Node>, ParseError> {
    let mut stack: Vec<(String, Vec<String>, usize, Vec<Node>)> = Vec::new();
    let mut root = Vec::new();

    for (line, text) in logical_lines(source) {
        if let Some(inner) = text.strip_prefix("</").and_then(|t| t.strip_suffix('>')) {
            let name = inner.trim().to_ascii_lowercase();
            match stack.pop() {
                Some((open, args, at, children)) if open.to_ascii_lowercase() == name => {
                    let node = Node::Block { name: open, args, line: at, children };
                    match stack.last_mut() {
                        Some(parent) => parent.3.push(node),
                        None => root.push(node),
                    }
                }
                _ => return Err(ParseError(format!("line {line}: unexpected </{}>", inner.trim()))),
            }
            continue;
        }
        if let Some(inner) = text.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            let inner = inner.trim();
            let (name, rest) = inner.split_once(char::is_whitespace).unwrap_or((inner, ""));
            stack.push((name.to_string(), split_args(rest), line, Vec::new()));
            continue;
        }
        let (name, rest) = text.split_once(char::is_whitespace).unwrap_or((&text, ""));
        let node = Node::Directive { name: name.to_string(), args: split_args(rest.trim()), line };
        match stack.last_mut() {
            Some(parent) => parent.3.push(node),
            None => root.push(node),
        }
    }

    if let Some((name, _, line, _)) = stack.last() {
        return Err(ParseError(format!("line {line}: <{name}> is never closed")));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_escapes() {
        assert_eq!(split_args(r#"set X-A "a b" 'c d' e\ f"#), ["set", "X-A", "a b", "c d", "e f"]);
        assert_eq!(split_args(r#"set X ''"#), ["set", "X", ""]);
    }

    #[test]
    fn regex_backslashes_survive_tokenizing() {
        // A stripped backslash turns ^\. into ^. and makes FilesMatch match every file.
        assert_eq!(split_args(r#""^\.""#), [r#"^\."#]);
        assert_eq!(split_args(r#""\.(bak|sql)$""#), [r#"\.(bak|sql)$"#]);
        // Quotes and a literal backslash are still unescaped.
        assert_eq!(split_args(r#""a\"b" "c\\d""#), ["a\"b", r"c\d"]);
    }

    #[test]
    fn blocks_comments_and_continuations() {
        let nodes = parse("# c\n<Files \".env\">\n  Require all \\\n denied\n</Files>\n").unwrap();
        match &nodes[0] {
            Node::Block { name, args, children, .. } => {
                assert_eq!(name, "Files");
                assert_eq!(args, &[".env"]);
                assert_eq!(children[0], Node::Directive { name: "Require".into(), args: vec!["all".into(), "denied".into()], line: 3 });
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn structural_errors() {
        assert!(parse("<IfModule x>\nHeader set A b\n").is_err());
        assert!(parse("</Files>\n").is_err());
        assert!(parse("<Files a>\n</IfModule>\n").is_err());
    }
}
