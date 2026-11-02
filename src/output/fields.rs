// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! `--fields a,b,c`: keep only the listed keys in each record (CLI Standard
//! §3), so an agent pays only for what it reads. `recall --fields
//! id,content` drops scope, agent, role and both timestamps from every
//! memory; content is most of the bytes, but the rest is paid on every row.
//!
//! What counts as a record depends on the shape of `data`:
//! - an array (`recall`, `search`): each object in it;
//! - an object holding arrays of objects (`context`, `rule list`,
//!   `ingest --list`): the objects in those arrays, while the object's own
//!   fields (`scope`, `budget`, `count`, …) are left alone;
//! - any other object (`remember`): the object itself.
//!
//! Kept keys come out in the order requested, which is also CSV's column
//! order. A listed key a record lacks is simply absent from it: engram
//! omits empty fields, so absence is not an error. The envelope records the
//! list as `metadata.fields`, so a reader can tell output was filtered.

use serde_json::{Map, Value};
use std::sync::OnceLock;

/// Set once in `main` from the global `--fields` flag, before any output.
/// Process-wide for the same reason as the output mode's emitters: every
/// command path ends in the shared `emit_ok`, and only it reads this.
static FIELDS: OnceLock<Vec<String>> = OnceLock::new();

/// Records the requested fields. Called at most once, at startup.
pub fn set(fields: Vec<String>) {
    let _ = FIELDS.set(fields);
}

/// The requested fields, if `--fields` was given.
pub fn get() -> Option<&'static [String]> {
    FIELDS.get().map(Vec::as_slice)
}

/// Splits and checks the flag's value: comma-separated, whitespace trimmed,
/// no empty names, duplicates removed in order.
///
/// # Errors
///
/// A message naming the problem when the list is empty or has an empty
/// entry (`id,,content`).
pub fn parse(raw: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for name in raw.split(',').map(str::trim) {
        if name.is_empty() {
            return Err(format!("--fields '{raw}' has an empty field name"));
        }
        if !out.iter().any(|f| f == name) {
            out.push(name.to_owned());
        }
    }
    Ok(out)
}

/// Applies `fields` to a serialized `Response` in place.
pub fn apply(envelope: &mut Value, fields: &[String]) {
    if let Some(data) = envelope.get_mut("data") {
        match data {
            Value::Array(items) => items.iter_mut().for_each(|r| select(r, fields)),
            Value::Object(map) if is_container(map) => {
                for child in map.values_mut() {
                    if let Value::Array(items) = child {
                        items.iter_mut().for_each(|r| select(r, fields));
                    }
                }
            }
            Value::Object(_) => select(data, fields),
            _ => {}
        }
    }
    if let Some(Value::Object(metadata)) = envelope.get_mut("metadata") {
        metadata.insert(
            "fields".to_owned(),
            Value::Array(fields.iter().cloned().map(Value::String).collect()),
        );
    }
}

/// True when a `data` object holds records rather than being one: it has an
/// array field that is empty or contains an object. Empty counts, so an
/// empty `rule list` or `context` keeps its own fields (`scope`, `count`,
/// `budget`) instead of being filtered as if it were a record and coming
/// back as `{}` (Codex and CodeRabbit on #34). An array of scalars, such as
/// a record's own tag list, does not make its object a container.
pub fn is_container(map: &Map<String, Value>) -> bool {
    map.values().any(|child| match child {
        Value::Array(items) => items.is_empty() || items.iter().any(Value::is_object),
        _ => false,
    })
}

/// Keeps only `fields` of one record, in the order given. Non-objects are
/// left as they are.
fn select(record: &mut Value, fields: &[String]) {
    if let Value::Object(map) = record {
        let mut kept = Map::new();
        for name in fields {
            if let Some(value) = map.remove(name) {
                kept.insert(name.clone(), value);
            }
        }
        *map = kept;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn records_in_an_array_keep_only_the_listed_keys_in_order() {
        let mut v = json!({
            "metadata": {"tool": "engram"},
            "data": [
                {"id": "a", "agent": "x", "content": "one", "scope": "s"},
                {"id": "b", "content": "two"},
            ],
        });
        apply(&mut v, &names(&["content", "id", "missing"]));
        assert_eq!(
            v["data"],
            json!([{"content": "one", "id": "a"}, {"content": "two", "id": "b"}])
        );
        let keys: Vec<&String> = v["data"][0].as_object().expect("record").keys().collect();
        assert_eq!(keys, ["content", "id"], "requested order, not record order");
        assert_eq!(v["metadata"]["fields"], json!(["content", "id", "missing"]));
    }

    #[test]
    fn a_container_object_filters_its_records_and_keeps_its_own_fields() {
        let mut v = json!({
            "metadata": {},
            "data": {
                "scope": "engram",
                "budget": {"requested_tokens": 300},
                "rules": [{"rule_id": "r", "content": "c", "scope": "engram"}],
                "memories": [{"id": "m", "content": "c", "role": "note"}],
                "tags": ["kept", "as", "is"],
            },
        });
        apply(&mut v, &names(&["id", "content"]));
        assert_eq!(
            v["data"],
            json!({
                "scope": "engram",
                "budget": {"requested_tokens": 300},
                "rules": [{"content": "c"}],
                "memories": [{"id": "m", "content": "c"}],
                "tags": ["kept", "as", "is"],
            })
        );
    }

    #[test]
    fn an_empty_result_keeps_its_container_fields() {
        let mut v = json!({"metadata": {}, "data": {
            "scope": "engram", "count": 0, "rules": [],
        }});
        apply(&mut v, &names(&["id", "content"]));
        assert_eq!(
            v["data"],
            json!({"scope": "engram", "count": 0, "rules": []})
        );

        let mut v = json!({"metadata": {}, "data": {
            "scope": "x", "budget": {"requested_tokens": 10}, "rules": [], "memories": [],
        }});
        apply(&mut v, &names(&["id"]));
        assert_eq!(v["data"]["scope"], "x");
        assert_eq!(v["data"]["budget"]["requested_tokens"], 10);
    }

    #[test]
    fn a_single_record_object_is_filtered_itself() {
        let mut v = json!({"metadata": {}, "data": {"id": "a", "content": "c", "agent": "x"}});
        apply(&mut v, &names(&["id"]));
        assert_eq!(v["data"], json!({"id": "a"}));
    }

    #[test]
    fn parsing_trims_dedupes_and_rejects_empty_names() {
        assert_eq!(
            parse(" id, content ,id").expect("valid"),
            names(&["id", "content"])
        );
        assert!(parse("id,,content").is_err());
        assert!(parse("").is_err());
        assert!(parse(",").is_err());
    }
}
