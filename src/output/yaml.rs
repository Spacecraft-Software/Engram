// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! `--format yaml`: the JSON envelope rendered as a YAML 1.2 block document.
//!
//! Hand-written rather than pulled in as a dependency. `serde_yaml` is
//! archived, and engram only ever *writes* YAML from a `serde_json::Value`,
//! which needs no parser and no type-driven serializer. The one hard part of
//! emitting YAML is quoting, and this sidesteps it: every string value is a
//! double-quoted scalar, so no content (`yes`, `null`, `- x`, `key: value`,
//! a leading `#`) can be read back as anything but the string it was.
//! [`quote`] escapes exactly what YAML double-quoted scalars cannot hold
//! raw. Tests parse the output with a real YAML 1.2 parser and compare it
//! with the JSON it came from.

use serde_json::Value;
use std::fmt::Write;

/// Renders `value` as a YAML document ending in a newline. Collections use
/// block style, indented two spaces per level; empty ones are written `{}`
/// and `[]`.
pub fn to_yaml(value: &Value) -> String {
    let mut out = String::new();
    match value {
        Value::Object(map) if !map.is_empty() => write_block(&mut out, value, 0),
        Value::Array(items) if !items.is_empty() => write_block(&mut out, value, 0),
        scalar => {
            out.push_str(&inline(scalar));
            out.push('\n');
        }
    }
    out
}

/// Writes a non-empty object or array, one entry per line, at `indent`.
fn write_block(out: &mut String, value: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let _ = write!(out, "{pad}{}:", key_text(key));
                if is_block(child) {
                    out.push('\n');
                    write_block(out, child, indent + 2);
                } else {
                    let _ = writeln!(out, " {}", inline(child));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                if is_block(item) {
                    // Render the child one level deeper, then turn its first
                    // line's indentation into this level's "- " marker:
                    // `  a: 1\n  b: 2` becomes `- a: 1\n  b: 2`.
                    let mut child = String::new();
                    write_block(&mut child, item, indent + 2);
                    out.push_str(&pad);
                    out.push_str("- ");
                    out.push_str(&child[indent + 2..]);
                } else {
                    let _ = writeln!(out, "{pad}- {}", inline(item));
                }
            }
        }
        scalar => {
            let _ = writeln!(out, "{pad}{}", inline(scalar));
        }
    }
}

/// True for a collection with entries, which needs lines of its own.
fn is_block(value: &Value) -> bool {
    match value {
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => false,
    }
}

/// A value that fits on its key's line: a scalar or an empty collection.
fn inline(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        // serde_json's number text always matches the YAML 1.2 core schema's
        // int or float pattern, so it reads back as the same number.
        Value::Number(n) => n.to_string(),
        Value::String(s) => quote(s),
        Value::Object(_) => "{}".to_owned(),
        Value::Array(_) => "[]".to_owned(),
    }
}

/// Words a YAML 1.1 parser reads as a boolean or null. YAML 1.2 does not,
/// but plenty of deployed parsers are 1.1, so a key spelled like one is
/// quoted.
const AMBIGUOUS_WORDS: &[&str] = &["y", "n", "yes", "no", "on", "off", "true", "false", "null"];

/// A key stays plain when it is an identifier no parser could misread
/// (every key engram emits is snake_case); anything else is quoted.
fn key_text(key: &str) -> String {
    let mut chars = key.chars();
    let identifier = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if identifier && !AMBIGUOUS_WORDS.iter().any(|w| w.eq_ignore_ascii_case(key)) {
        key.to_owned()
    } else {
        quote(key)
    }
}

/// A YAML double-quoted scalar holding `s` exactly.
///
/// Raw characters must be YAML-printable (`c-printable`). Everything else is
/// escaped: C0 controls and DEL, the C1 block, U+FFFE/U+FFFF, and U+2028/
/// U+2029 and U+0085, which YAML 1.1 parsers treat as line breaks. All other
/// Unicode is written as-is, since output is UTF-8.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{fffe}'
            | '\u{ffff}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use yaml_rust2::{Yaml, YamlLoader};

    /// Parses `text` with a YAML 1.2 parser and converts it back to JSON.
    fn parse(text: &str) -> Value {
        let docs = YamlLoader::load_from_str(text)
            .unwrap_or_else(|e| panic!("invalid YAML ({e}):\n{text}"));
        assert_eq!(docs.len(), 1, "one document:\n{text}");
        to_json(&docs[0])
    }

    fn to_json(y: &Yaml) -> Value {
        match y {
            Yaml::Null => Value::Null,
            Yaml::Boolean(b) => Value::Bool(*b),
            Yaml::Integer(i) => json!(i),
            Yaml::Real(r) => {
                let f: f64 = r.parse().expect("YAML real");
                json!(f)
            }
            Yaml::String(s) => Value::String(s.clone()),
            Yaml::Array(items) => Value::Array(items.iter().map(to_json).collect()),
            Yaml::Hash(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| {
                        let key = k.as_str().expect("string key").to_owned();
                        (key, to_json(v))
                    })
                    .collect(),
            ),
            other => panic!("unexpected YAML node {other:?}"),
        }
    }

    fn round_trips(value: &Value) {
        let text = to_yaml(value);
        assert_eq!(&parse(&text), value, "YAML:\n{text}");
    }

    #[test]
    fn an_envelope_round_trips_through_a_yaml_parser() {
        round_trips(&json!({
            "metadata": {
                "tool": "engram",
                "command": "engram recall --scope x",
                "timestamp": "2026-11-01T12:00:00Z",
                "budget": null,
                "count": 2,
                "ratio": 0.25,
                "ok": true,
            },
            "data": [
                {"id": "a", "content": "first", "rule_id": null, "tags": []},
                {"id": "b", "content": "second", "nested": {"k": [1, [2, 3], {}]}},
            ],
        }));
    }

    #[test]
    fn strings_that_look_like_yaml_stay_strings() {
        let tricky = [
            "yes",
            "No",
            "on",
            "null",
            "~",
            "true",
            "1.5",
            "0x1f",
            "1e3",
            "- item",
            "key: value",
            "# comment",
            "&anchor",
            "*alias",
            "!tag",
            "|",
            ">",
            "@at",
            "`tick",
            "'single'",
            "\"double\"",
            " leading space",
            "trailing space ",
            "",
            "multi\nline\n\ntext",
            "tab\there",
            "back\\slash",
            "c0 \u{1} del \u{7f} c1 \u{85}\u{9f} ls \u{2028}",
            "emoji 🦀 and Ελληνικά",
            "{flow: map}",
            "[flow, seq]",
            "---",
            "...",
        ];
        let value = json!({
            "values": tricky.to_vec(),
            "as keys": tricky.iter().map(|k| (k.to_string(), json!(1))).collect::<serde_json::Map<_, _>>(),
        });
        round_trips(&value);
    }

    #[test]
    fn empty_collections_and_scalars_at_the_top_level() {
        round_trips(&json!({}));
        round_trips(&json!([]));
        round_trips(&json!("just a string"));
        round_trips(&json!(null));
        round_trips(&json!([[1, 2], [[]], [{"a": [{}]}]]));
    }

    #[test]
    fn the_layout_is_readable_block_style() {
        let text = to_yaml(&json!({
            "metadata": {"tool": "engram"},
            "data": [{"id": "a", "role": "user"}, "plain"],
        }));
        assert_eq!(
            text,
            "metadata:\n  tool: \"engram\"\ndata:\n  - id: \"a\"\n    role: \"user\"\n  - \"plain\"\n"
        );
    }

    #[test]
    fn ambiguous_keys_are_quoted() {
        assert_eq!(key_text("scope_origin"), "scope_origin");
        assert_eq!(key_text("on"), "\"on\"");
        assert_eq!(key_text("No"), "\"No\"");
        assert_eq!(key_text("with space"), "\"with space\"");
        assert_eq!(key_text("1st"), "\"1st\"");
    }
}
