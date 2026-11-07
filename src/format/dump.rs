//! Serializes [`Value`]s exactly like js-yaml 5.4.3's
//! `dump(data, { schema: YAML11_SCHEMA })` with default options: block style,
//! 2-space indent, 80-column folding, single quotes preferred.
//!
//! The structure mirrors js-yaml's presenter (`writeNode`, `writeBlockMapping`,
//! `writeBlockSequence`) and scalar styler (`detectAllowedStyles` plus the
//! default style rules) so the output matches byte for byte.

use super::js::{self, utf16_len};
use super::resolve::{Kind, plain_kind};
use super::value::{Map, Value};

const INDENT: usize = 2;
const LINE_WIDTH: usize = 80;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Plain,
    Single,
    Double,
    Literal,
    Folded,
}

#[derive(Default)]
struct State {
    /// The last scalar written was a block scalar ending in a blank line, so
    /// the document needs an explicit `...` end marker.
    open_ended: bool,
}

#[derive(Clone, Copy, Default)]
struct Ctx {
    block: bool,
    compact: bool,
    is_key: bool,
}

/// `dump(map)`: the YAML text including its trailing newline.
pub fn dump(map: &Map) -> String {
    let mut state = State::default();
    let body = write_map_node(
        &mut state,
        0,
        map,
        Ctx {
            block: true,
            compact: true,
            is_key: false,
        },
    );
    let mut out = body + "\n";
    if state.open_ended {
        out.push_str("...\n");
    }
    out
}

fn next_line(level: usize) -> String {
    format!("\n{}", " ".repeat(INDENT * level))
}

fn write_node(state: &mut State, level: usize, value: &Value, ctx: Ctx) -> String {
    match value {
        Value::Map(map) => write_map_node(state, level, map, ctx),
        Value::Seq(items) => {
            if ctx.block && !items.is_empty() {
                write_block_sequence(state, level, items, ctx.compact)
            } else {
                state.open_ended = false;
                let inner: Vec<String> = items
                    .iter()
                    .map(|item| write_node(state, level, item, Ctx::default()))
                    .collect();
                format!("[{}]", inner.join(", "))
            }
        }
        scalar => {
            let (text, kind) = represent(scalar);
            write_scalar(state, level, &text, kind, ctx)
        }
    }
}

fn write_map_node(state: &mut State, level: usize, map: &Map, ctx: Ctx) -> String {
    if ctx.block && !map.is_empty() {
        return write_block_mapping(state, level, map, ctx.compact);
    }
    state.open_ended = false;
    let pairs: Vec<String> = map
        .iter()
        .map(|(k, v)| {
            let key = write_scalar(
                state,
                level,
                k,
                Kind::Str,
                Ctx {
                    is_key: true,
                    ..Ctx::default()
                },
            );
            let value = write_node(state, level, v, Ctx::default());
            let sep = if value.is_empty() { "" } else { " " };
            format!("{key}:{sep}{value}")
        })
        .collect();
    format!("{{{}}}", pairs.join(", "))
}

fn write_block_sequence(state: &mut State, level: usize, items: &[Value], compact: bool) -> String {
    let mut result = String::new();
    for item in items {
        let text = write_node(
            state,
            level + 1,
            item,
            Ctx {
                block: true,
                compact: true,
                is_key: false,
            },
        );
        if !compact || !result.is_empty() {
            result += &next_line(level);
        }
        result += if text.is_empty() || text.starts_with('\n') {
            "-"
        } else {
            "- "
        };
        result += &text;
    }
    result
}

fn write_block_mapping(state: &mut State, level: usize, map: &Map, compact: bool) -> String {
    let mut result = String::new();
    for (key, value) in map {
        let mut pair = String::new();
        if !compact || !result.is_empty() {
            pair += &next_line(level);
        }
        let key_text = write_scalar(
            state,
            level + 1,
            key,
            Kind::Str,
            Ctx {
                block: true,
                compact: true,
                is_key: true,
            },
        );
        let explicit = key.contains('\n') || utf16_len(&key_text) > 1024;
        if explicit {
            pair += if key_text.starts_with('\n') {
                "?"
            } else {
                "? "
            };
        }
        pair += &key_text;
        if explicit {
            pair += &next_line(level);
        }
        let value_text = write_node(
            state,
            level + 1,
            value,
            Ctx {
                block: true,
                compact: explicit,
                is_key: false,
            },
        );
        pair += if value_text.is_empty() || value_text.starts_with('\n') {
            ":"
        } else {
            ": "
        };
        pair += &value_text;
        result += &pair;
    }
    result
}

/// js-yaml's `represent` for scalars: the text and the tag it's written under.
fn represent(value: &Value) -> (String, Kind) {
    match value {
        Value::Null => ("null".into(), Kind::Null),
        Value::Bool(b) => (b.to_string(), Kind::Bool),
        Value::String(s) => (s.clone(), Kind::Str),
        Value::Date(ms) => (js::iso_string(*ms), Kind::Timestamp),
        Value::Number(n) => {
            let text = js::number_to_string(*n);
            let is_int =
                n.fract() == 0.0 && !(*n == 0.0 && n.is_sign_negative()) && !text.contains('e');
            if is_int {
                (text, Kind::Int)
            } else {
                (represent_float(*n), Kind::Float)
            }
        }
        Value::Seq(_) | Value::Map(_) => unreachable!("collections are not scalars"),
    }
}

fn represent_float(n: f64) -> String {
    if n.is_nan() {
        return ".nan".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { ".inf" } else { "-.inf" }.into();
    }
    if n == 0.0 && n.is_sign_negative() {
        return "-0.0".into();
    }
    let text = js::number_to_string(n);
    let digits_then_e = text
        .trim_start_matches(['-', '+'])
        .split_once('e')
        .is_some_and(|(mantissa, _)| {
            !mantissa.is_empty() && mantissa.chars().all(|c| c.is_ascii_digit())
        });
    if digits_then_e {
        text.replacen('e', ".e", 1)
    } else {
        text
    }
}

struct Layout<'a> {
    value: &'a str,
    kind: Kind,
    is_key: bool,
    flow_only: bool,
    shift_of_parent: isize,
    shift_of_content: usize,
    shift_of_first_line: usize,
}

fn write_scalar(state: &mut State, level: usize, value: &str, kind: Kind, ctx: Ctx) -> String {
    let layout = Layout {
        value,
        kind,
        is_key: ctx.is_key,
        flow_only: !ctx.block,
        shift_of_parent: if level == 0 {
            -1
        } else {
            (INDENT * (level - 1)) as isize
        },
        shift_of_content: INDENT * level.max(1),
        shift_of_first_line: if level == 0 { 0 } else { INDENT * level },
    };
    let allowed = Allowed {
        plain: can_use_plain(&layout),
        single: can_use_single(&layout),
        block: can_use_block(&layout),
    };
    let style = choose_style(&layout, &allowed);
    state.open_ended = matches!(style, Style::Literal | Style::Folded)
        && (value == "\n" || value.ends_with("\n\n"));
    render(&layout, style)
}

struct Allowed {
    plain: bool,
    single: bool,
    block: bool,
}

impl Allowed {
    fn has(&self, style: Style) -> bool {
        match style {
            Style::Plain => self.plain,
            Style::Single => self.single,
            Style::Literal | Style::Folded => self.block,
            Style::Double => true,
        }
    }
}

/// js-yaml's default scalar style rules, applied in order starting from plain.
fn choose_style(layout: &Layout<'_>, allowed: &Allowed) -> Style {
    let value = layout.value;
    let mut style = Style::Plain;
    // doubleQuoteForInvisibles
    if value.chars().any(|c| {
        matches!(
            c,
            '\t' | '\u{7F}'
                ..='\u{A0}' | '\u{2028}' | '\u{2029}' | '\u{FEFF}' | '\u{FFFE}' | '\u{FFFF}'
        )
    }) {
        style = Style::Double;
    }
    // doubleQuoteWhitespaceOnly
    if style == Style::Plain && !value.is_empty() && value.chars().all(js::is_js_space) {
        style = Style::Double;
    }
    // tryLongOrMultilineAsBlock
    if style == Style::Plain && !layout.is_key {
        let multiline = value.contains('\n');
        if !allowed.block {
            if multiline {
                style = Style::Double;
            }
        } else {
            let width = LINE_WIDTH
                .min(40)
                .max(LINE_WIDTH.saturating_sub(layout.shift_of_content));
            let should_fold = value.split('\n').any(|line| {
                utf16_len(line) > width && !line.starts_with(' ') && has_fold_point(line)
            });
            if should_fold {
                style = Style::Folded;
            } else if multiline {
                style = Style::Literal;
            }
        }
    }
    // quoteInvalidPlain (quoteStyle "single")
    if style == Style::Plain && !allowed.plain {
        style = if allowed.single {
            Style::Single
        } else {
            Style::Double
        };
    }
    // fallbackToDoubleQuoted
    if !allowed.has(style) {
        style = Style::Double;
    }
    style
}

/// `/ [^ \t]/.test(line)`: a space followed by a non-blank character.
fn has_fold_point(line: &str) -> bool {
    line.as_bytes()
        .windows(2)
        .any(|w| w[0] == b' ' && w[1] != b' ' && w[1] != b'\t')
}

// --- character classes from the YAML 1.2.2 spec, as js-yaml defines them ---

fn is_printable(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | ' '..='~' | '\u{85}' | '\u{A0}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
}

fn is_nb_char(c: char) -> bool {
    !matches!(c, '\n' | '\r' | '\u{FEFF}') && is_printable(c)
}

fn is_ns_char(c: char) -> bool {
    !matches!(c, ' ' | '\t') && is_nb_char(c)
}

fn is_indicator(c: char) -> bool {
    matches!(
        c,
        '-' | '?'
            | ':'
            | ','
            | '['
            | ']'
            | '{'
            | '}'
            | '#'
            | '&'
            | '*'
            | '!'
            | '|'
            | '>'
            | '\''
            | '"'
            | '%'
            | '@'
            | '`'
    )
}

fn is_flow_indicator(c: char) -> bool {
    matches!(c, ',' | '[' | ']' | '{' | '}')
}

/// Matches js-yaml's `NS_PLAIN_*` regexes: whether `s` can be written as a
/// plain scalar in the given context.
fn matches_ns_plain(s: &str, flow_in: bool, multi_line: bool) -> bool {
    let safe = |c: char| is_ns_char(c) && !(flow_in && is_flow_indicator(c));
    let chars: Vec<char> = s.chars().collect();
    let at = |i: usize| chars.get(i).copied();
    // ns-plain-char: a safe char other than ':' and '#', or ':' followed by a safe char
    let plain_char = |i: usize| match at(i) {
        Some(':') => at(i + 1).is_some_and(safe),
        Some('#') | None => false,
        Some(c) => safe(c),
    };

    let mut i = match at(0) {
        Some(c) if is_ns_char(c) && !is_indicator(c) => 1,
        Some('?' | ':' | '-') if at(1).is_some_and(safe) => 1,
        _ => return false,
    };
    loop {
        // '#' is allowed directly after a plain character
        while at(i) == Some('#') {
            i += 1;
        }
        match at(i) {
            None => return true,
            Some(' ' | '\t') => {
                while matches!(at(i), Some(' ' | '\t')) {
                    i += 1;
                }
                if !plain_char(i) {
                    return false;
                }
                i += 1;
            }
            Some('\n') if multi_line => {
                while at(i) == Some('\n') {
                    i += 1;
                }
                if !plain_char(i) {
                    return false;
                }
                i += 1;
            }
            Some(_) => {
                if !plain_char(i) {
                    return false;
                }
                i += 1;
            }
        }
    }
}

fn starts_with_document_marker(s: &str) -> bool {
    (s.starts_with("---") || s.starts_with("..."))
        && matches!(s[3..].chars().next(), None | Some(' ' | '\t' | '\n' | '\r'))
}

fn can_use_plain(layout: &Layout<'_>) -> bool {
    let s = layout.value;
    if !s.is_empty() {
        let ok = if layout.is_key {
            matches_ns_plain(s, layout.flow_only, false)
        } else {
            matches_ns_plain(s, layout.flow_only, true)
        };
        if !ok {
            return false;
        }
        if layout.shift_of_first_line == 0 && starts_with_document_marker(s) {
            return false;
        }
    }
    let resolved = plain_kind(s);
    if resolved != layout.kind {
        return false;
    }
    !(s == "=" && resolved == Kind::Str)
}

fn can_use_single(layout: &Layout<'_>) -> bool {
    let s = layout.value;
    let nb_json = |c: char| matches!(c, '\t' | ' '..='\u{D7FF}' | '\u{E000}'..);
    let ok = if layout.is_key {
        s.chars().all(nb_json)
    } else {
        s.chars().all(|c| c == '\n' || nb_json(c))
    };
    ok && !s.contains(" \n") && !s.contains("\t\n") && !s.contains("\n ") && !s.contains("\n\t")
}

fn can_use_block(layout: &Layout<'_>) -> bool {
    if layout.flow_only || !layout.value.chars().all(|c| c == '\n' || is_nb_char(c)) {
        return false;
    }
    let content_indent = layout.shift_of_content as isize - layout.shift_of_parent;
    if content_indent < 1 {
        return false;
    }
    !(content_indent > 9 && needs_indent_indicator(layout.value))
}

fn needs_indent_indicator(s: &str) -> bool {
    s.trim_start_matches('\n').starts_with(' ')
}

fn render(layout: &Layout<'_>, style: Style) -> String {
    let value = layout.value;
    match style {
        Style::Plain => encode_flow_breaks(value, layout.shift_of_content),
        Style::Single => format!(
            "'{}'",
            encode_flow_breaks(value, layout.shift_of_content).replace('\'', "''")
        ),
        Style::Double => format!("\"{}\"", escape_double(value)),
        Style::Literal => format!(
            "|{}{}",
            block_header(value, layout),
            drop_ending_newline(&indent_string(value, layout.shift_of_content))
        ),
        Style::Folded => {
            let width = LINE_WIDTH
                .min(40)
                .max(LINE_WIDTH.saturating_sub(layout.shift_of_content));
            format!(
                ">{}{}",
                block_header(value, layout),
                drop_ending_newline(&indent_string(
                    &fold_block_scalar(value, width),
                    layout.shift_of_content
                ))
            )
        }
    }
}

fn encode_flow_breaks(s: &str, shift: usize) -> String {
    let Some(first) = s.find('\n') else {
        return s.to_string();
    };
    let pad = " ".repeat(shift);
    let mut result = s[..first].to_string();
    let rest = &s[first..];
    let mut i = 0;
    let bytes = rest.as_bytes();
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && bytes[i] == b'\n' {
            i += 1;
        }
        let breaks = i - start;
        let line_start = i;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        result += &"\n".repeat(breaks + 1);
        result += &pad;
        result += &rest[line_start..i];
    }
    result
}

fn indent_string(s: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);
    let mut result = String::with_capacity(s.len() + spaces * 4);
    for line in s.split_inclusive('\n') {
        if line != "\n" {
            result += &indent;
        }
        result += line;
    }
    result
}

fn block_header(s: &str, layout: &Layout<'_>) -> String {
    let indicator = if needs_indent_indicator(s) {
        (layout.shift_of_content as isize - layout.shift_of_parent).to_string()
    } else {
        String::new()
    };
    let clip = s.ends_with('\n');
    let chomp = if clip && (s.ends_with("\n\n") || s == "\n") {
        "+"
    } else if clip {
        ""
    } else {
        "-"
    };
    format!("{indicator}{chomp}\n")
}

fn drop_ending_newline(s: &str) -> &str {
    s.strip_suffix('\n').unwrap_or(s)
}

fn is_more_indented(c: Option<char>) -> bool {
    matches!(c, Some(' ' | '\t'))
}

/// js-yaml's `foldLine`. Positions are UTF-16 offsets, as in JS; the line is
/// only ever cut at ASCII spaces, so slicing the UTF-16 units is safe.
fn fold_line(line: &str, width: usize) -> String {
    if line.is_empty() || is_more_indented(line.chars().next()) {
        return line.to_string();
    }
    let units: Vec<u16> = line.encode_utf16().collect();
    let slice = |a: usize, b: usize| String::from_utf16_lossy(&units[a..b]);
    let space = u16::from(b' ');
    let tab = u16::from(b'\t');
    let (mut start, mut curr) = (0usize, 0usize);
    let mut result = String::new();
    for next in 0..units.len().saturating_sub(1) {
        if units[next] != space || units[next + 1] == space || units[next + 1] == tab {
            continue;
        }
        if next - start > width {
            let end = if curr > start { curr } else { next };
            result.push('\n');
            result += &slice(start, end);
            start = end + 1;
        }
        curr = next;
    }
    result.push('\n');
    if units.len() - start > width && curr > start {
        result += &slice(start, curr);
        result.push('\n');
        result += &slice(curr + 1, units.len());
    } else {
        result += &slice(start, units.len());
    }
    result[1..].to_string()
}

/// js-yaml's `foldBlockScalar`.
fn fold_block_scalar(s: &str, width: usize) -> String {
    let first_end = s.find('\n').unwrap_or(s.len());
    let mut result = fold_line(&s[..first_end], width);
    let mut prev_more_indented = s.starts_with('\n') || is_more_indented(s.chars().next());
    let rest = &s[first_end..];
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let prefix_start = i;
        while i < bytes.len() && bytes[i] == b'\n' {
            i += 1;
        }
        let prefix = &rest[prefix_start..i];
        let line_start = i;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        let line = &rest[line_start..i];
        let more_indented = !line.is_empty() && is_more_indented(line.chars().next());
        result += prefix;
        if !prev_more_indented && !more_indented && !line.is_empty() {
            result.push('\n');
        }
        result += &fold_line(line, width);
        prev_more_indented = more_indented;
    }
    result
}

fn escape_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\0' => out += "\\0",
            '\u{07}' => out += "\\a",
            '\u{08}' => out += "\\b",
            '\t' => out += "\\t",
            '\n' => out += "\\n",
            '\u{0B}' => out += "\\v",
            '\u{0C}' => out += "\\f",
            '\r' => out += "\\r",
            '\u{1B}' => out += "\\e",
            '"' => out += "\\\"",
            '\\' => out += "\\\\",
            '\u{85}' => out += "\\N",
            '\u{A0}' => out += "\\_",
            '\u{2028}' => out += "\\L",
            '\u{2029}' => out += "\\P",
            '\u{00}'..='\u{1F}' | '\u{7F}'..='\u{9F}' => out += &format!("\\x{:02X}", c as u32),
            '\u{FEFF}' | '\u{FFFE}' | '\u{FFFF}' => out += &format!("\\u{:04X}", c as u32),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(key: &str, value: Value) -> String {
        let mut map = Map::new();
        map.insert(key.into(), value);
        dump(&map)
    }

    #[test]
    fn strings_that_look_typed_should_be_single_quoted() {
        assert_eq!(one("due", "2026-10-19".into()), "due: '2026-10-19'\n");
    }

    #[test]
    fn colon_space_should_force_quotes() {
        assert_eq!(
            one("title", "TUI-DO Core: background".into()),
            "title: 'TUI-DO Core: background'\n"
        );
    }

    #[test]
    fn empty_sequences_should_be_flow() {
        assert_eq!(one("attachments", Value::Seq(vec![])), "attachments: []\n");
    }

    #[test]
    fn sequences_should_indent_under_the_key() {
        assert_eq!(one("a", Value::Seq(vec!["x".into()])), "a:\n  - x\n");
    }

    #[test]
    fn long_values_should_fold() {
        let title = format!(
            "{} w",
            ["word0"; 1]
                .iter()
                .cycle()
                .take(13)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(one("title", title.into()).starts_with("title: >-\n  "));
    }
}
