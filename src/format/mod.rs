//! The workspace file format: markdown with a YAML frontmatter block, read and
//! written exactly like TUI-DO's original JS implementation (js-yaml 5.4.3,
//! YAML 1.1 schema), so files stay byte-compatible.

mod dump;
pub mod js;
mod load;
mod resolve;
mod value;

pub use load::LoadError;
pub use value::{Map, Value, map_to_json};

const DELIMITER: &str = "---";

/// A parsed file: frontmatter data and the body after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub data: Map,
    pub content: String,
}

#[derive(Debug)]
pub enum ParseError {
    UnsupportedLanguage(String),
    Yaml(LoadError),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::UnsupportedLanguage(lang) => {
                write!(f, "Unsupported frontmatter language: {lang}")
            }
            ParseError::Yaml(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// js-yaml `dump(data, { schema: YAML11_SCHEMA })`.
pub fn dump_yaml(data: &Map) -> String {
    dump::dump(data)
}

/// js-yaml `load(source, { schema: YAML11_SCHEMA })`.
pub fn load_yaml(source: &str) -> Result<Option<Value>, LoadError> {
    load::load(source)
}

/// Split a file into frontmatter data and body. The body is returned as is;
/// readers that want it trimmed use [`parse_file`].
pub fn parse_frontmatter(raw: &str) -> Result<Document, ParseError> {
    let s = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
    // "----" is a horizontal rule, not frontmatter
    if !s.starts_with(DELIMITER) || s[DELIMITER.len()..].starts_with('-') {
        return Ok(Document {
            data: Map::new(),
            content: s.to_string(),
        });
    }
    let rest = &s[DELIMITER.len()..];
    let first_newline = rest.find('\n');
    let language = js::trim(first_newline.map_or(rest, |i| &rest[..i]));
    if !language.is_empty() && language != "yaml" && language != "yml" {
        return Err(ParseError::UnsupportedLanguage(language.to_string()));
    }

    let close = rest.find("\n---");
    let block_start = first_newline.map_or(rest.len(), |i| i + 1);
    let block_end = close.unwrap_or(rest.len());
    let block = if block_start < block_end {
        &rest[block_start..block_end]
    } else {
        ""
    };

    let content = match close {
        Some(i) => {
            // Skip the closing delimiter line
            let after = &rest[i + 1 + DELIMITER.len()..];
            after.find('\n').map_or("", |nl| &after[nl + 1..])
        }
        None => "",
    };

    let data = if js::trim(block).is_empty() {
        Map::new()
    } else {
        match load::load(block).map_err(ParseError::Yaml)? {
            Some(Value::Map(map)) => map,
            _ => Map::new(),
        }
    };
    Ok(Document {
        data,
        content: content.to_string(),
    })
}

/// Read a file the way the app does: frontmatter plus the body trimmed with
/// JS `trim` semantics.
pub fn parse_file(raw: &str) -> Result<Document, ParseError> {
    let mut doc = parse_frontmatter(raw)?;
    doc.content = js::trim(&doc.content).to_string();
    Ok(doc)
}

/// Serialize a file. An empty map writes no frontmatter block; the body gets
/// exactly one trailing newline if it doesn't already end in one.
pub fn stringify(content: &str, data: &Map) -> String {
    let yaml = if data.is_empty() {
        "{}".to_string()
    } else {
        js::trim(&dump::dump(data)).to_string()
    };
    let header = if yaml == "{}" {
        String::new()
    } else {
        format!("{DELIMITER}\n{yaml}\n{DELIMITER}\n")
    };
    let newline = if content.ends_with('\n') { "" } else { "\n" };
    format!("{header}{content}{newline}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_should_preserve_typical_task_file() {
        let raw = "---\nid: 26-955\ntitle: 'TUI-DO Core: background daemon'\nstate: in-progress\ndue: null\n---\n\nBody text\n";
        let doc = parse_file(raw).expect("parses");
        assert_eq!(stringify(&format!("\n{}", doc.content), &doc.data), raw);
    }

    #[test]
    fn horizontal_rule_should_not_be_frontmatter() {
        assert!(
            parse_frontmatter("----\ntext")
                .expect("parses")
                .data
                .is_empty()
        );
    }

    #[test]
    fn unsupported_language_should_fail() {
        assert!(parse_frontmatter("---js\nx\n---\n").is_err());
    }

    #[test]
    fn empty_data_should_write_no_header() {
        assert_eq!(stringify("body", &Map::new()), "body\n");
    }
}
