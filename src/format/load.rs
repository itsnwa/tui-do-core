//! Loads a YAML document into [`Value`]s with js-yaml's YAML 1.1 semantics:
//! plain scalars are typed (see `resolve`), quoted and block scalars are
//! strings, duplicate keys are an error, and `<<` merges mappings.

use std::collections::{HashMap, HashSet};

use saphyr_parser::{Event, Parser, ScalarStyle, Tag};

use super::resolve::{Resolved, resolve_plain};
use super::value::{Map, Value};

#[derive(Debug)]
pub struct LoadError(pub String);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

type Result<T> = std::result::Result<T, LoadError>;

enum Node {
    Value(Value),
    Merge,
}

struct Loader<'a> {
    parser: Parser<'a, saphyr_parser::StrInput<'a>>,
    anchors: HashMap<usize, Value>,
}

/// Load the single document in `source`. Empty input gives `None`.
pub fn load(source: &str) -> Result<Option<Value>> {
    let mut loader = Loader {
        parser: Parser::new_from_str(source),
        anchors: HashMap::new(),
    };
    let mut result = None;
    let mut documents = 0;
    loop {
        match loader.next()? {
            Event::StreamEnd => break,
            Event::DocumentStart(_) => {
                documents += 1;
                if documents > 1 {
                    return Err(LoadError("expected a single document in the stream".into()));
                }
                let event = loader.next()?;
                result = Some(loader.value(event)?);
            }
            _ => {}
        }
    }
    Ok(result)
}

fn is_str_tag(tag: Option<&Tag>) -> bool {
    tag.is_some_and(|t| t.suffix == "str")
}

impl<'a> Loader<'a> {
    fn next(&mut self) -> Result<Event<'a>> {
        match self.parser.next() {
            Some(Ok((event, _))) => Ok(event),
            Some(Err(err)) => Err(LoadError(err.to_string())),
            None => Ok(Event::StreamEnd),
        }
    }

    fn remember(&mut self, anchor: usize, value: &Value) {
        if anchor != 0 {
            self.anchors.insert(anchor, value.clone());
        }
    }

    fn value(&mut self, event: Event<'a>) -> Result<Value> {
        match self.node(event)? {
            Node::Value(v) => Ok(v),
            Node::Merge => Ok(Value::String("<<".into())),
        }
    }

    fn node(&mut self, event: Event<'a>) -> Result<Node> {
        match event {
            Event::Scalar(text, style, anchor, tag) => {
                let node = if style == ScalarStyle::Plain && !is_str_tag(tag.as_deref()) {
                    match resolve_plain(&text) {
                        Resolved::Value(v) => Node::Value(v),
                        Resolved::Merge => Node::Merge,
                    }
                } else {
                    Node::Value(Value::String(text.into_owned()))
                };
                if let Node::Value(v) = &node {
                    self.remember(anchor, v);
                }
                Ok(node)
            }
            Event::SequenceStart(anchor, _) => {
                let mut items = Vec::new();
                loop {
                    match self.next()? {
                        Event::SequenceEnd => break,
                        event => items.push(self.value(event)?),
                    }
                }
                let value = Value::Seq(items);
                self.remember(anchor, &value);
                Ok(Node::Value(value))
            }
            Event::MappingStart(anchor, _) => {
                let value = Value::Map(self.mapping()?);
                self.remember(anchor, &value);
                Ok(Node::Value(value))
            }
            Event::Alias(id) => self
                .anchors
                .get(&id)
                .cloned()
                .map(Node::Value)
                .ok_or_else(|| LoadError("unidentified alias".into())),
            other => Err(LoadError(format!("unexpected YAML event {other:?}"))),
        }
    }

    fn mapping(&mut self) -> Result<Map> {
        let mut map = Map::new();
        // Keys that came from a merge may be overridden without a duplicate error.
        let mut merged: HashSet<String> = HashSet::new();
        loop {
            let key_event = match self.next()? {
                Event::MappingEnd => break,
                event => event,
            };
            let key = self.node(key_event)?;
            let value_event = self.next()?;
            let value = self.value(value_event)?;
            match key {
                Node::Merge => {
                    let sources = match value {
                        Value::Map(m) => vec![m],
                        Value::Seq(items) => items
                            .into_iter()
                            .map(|item| match item {
                                Value::Map(m) => Ok(m),
                                _ => Err(LoadError("cannot merge mappings; the provided source object is unacceptable".into())),
                            })
                            .collect::<Result<_>>()?,
                        _ => return Err(LoadError("cannot merge mappings; the provided source object is unacceptable".into())),
                    };
                    for source in sources {
                        for (k, v) in source {
                            if !map.contains_key(&k) {
                                merged.insert(k.clone());
                                map.insert(k, v);
                            }
                        }
                    }
                }
                Node::Value(key) => {
                    let key = key.to_js_string();
                    if map.contains_key(&key) && !merged.remove(&key) {
                        return Err(LoadError(format!("duplicated mapping key ({key})")));
                    }
                    map.insert(key, value);
                }
            }
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(src: &str) -> Map {
        match load(src).expect("loads") {
            Some(Value::Map(m)) => m,
            other => panic!("not a map: {other:?}"),
        }
    }

    #[test]
    fn quoted_dates_should_stay_strings() {
        assert_eq!(
            map("due: '2026-03-01'")["due"],
            Value::String("2026-03-01".into())
        );
    }

    #[test]
    fn plain_dates_should_become_dates() {
        assert!(matches!(map("due: 2026-03-01")["due"], Value::Date(_)));
    }

    #[test]
    fn duplicate_keys_should_fail() {
        assert!(load("a: 1\na: 2").is_err());
    }

    #[test]
    fn merge_keys_should_merge_without_overriding() {
        let m = map("base: &b {a: 1, b: 2}\nchild:\n  <<: *b\n  b: 3");
        let Value::Map(child) = &m["child"] else {
            panic!("child is not a map")
        };
        assert_eq!(
            (child["a"].clone(), child["b"].clone()),
            (Value::Number(1.0), Value::Number(3.0))
        );
    }

    #[test]
    fn yaml11_bool_keys_should_become_js_strings() {
        assert!(map("y: 1").contains_key("true"));
    }

    #[test]
    fn folded_blocks_should_unfold() {
        assert_eq!(map("t: >-\n  a b\n  c")["t"], Value::String("a b c".into()));
    }
}
