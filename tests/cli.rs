// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Integration tests for the engram CLI surface.
//!
//! The test harness's stdout is never a TTY, so the output-mode cascade lands
//! on machine (json) mode by default; the human/TTY modes are unreachable
//! here and are deliberately not asserted. Every test scrubs the agent/CI
//! environment variables and points `--db` at a per-test temporary file so
//! the host environment cannot leak into the assertions.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

/// Env vars that would leak the host's agent/CI context into the mode
/// cascade, scope resolution, agent defaults, or `metadata.tool_agent`.
const HERMETIC_VARS: [&str; 12] = [
    "AI_AGENT",
    "AGENT",
    "CI",
    "NO_COLOR",
    "SPACECRAFT_A11Y",
    "ENGRAM_DB",
    "ENGRAM_SCOPE",
    "ENGRAM_AGENT",
    "ENGRAM_MODEL",
    "CLAUDECODE",
    "CURSOR_AGENT",
    "GEMINI_CLI",
];

/// A hermetic `engram` invocation against the given database file.
fn engram(db: &Path) -> Command {
    let mut cmd = Command::cargo_bin("engram").expect("engram binary builds");
    for var in HERMETIC_VARS {
        cmd.env_remove(var);
    }
    // Point the model cascade's XDG fallback at a directory that has no
    // engram/model in it, so a Model2Vec model installed on the developer's
    // machine (~/.local/share/engram/model) can never leak into a
    // vector-feature test run and silently flip search to hybrid.
    cmd.env(
        "XDG_DATA_HOME",
        db.parent().expect("db lives in a tempdir").join("xdg-data"),
    );
    // `--db` is a top-level (non-global) flag, so it must precede the
    // subcommand; the helper adds it first, callers append the rest.
    cmd.arg("--db").arg(db);
    cmd
}

fn parse_json(bytes: &[u8]) -> Value {
    let text = std::str::from_utf8(bytes).expect("output is UTF-8");
    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("output is not valid JSON ({e}): {text}"))
}

/// Asserts the bytes are exactly one non-empty line and parses it as JSON.
fn parse_single_line_json(bytes: &[u8]) -> Value {
    let text = std::str::from_utf8(bytes).expect("output is UTF-8");
    let trimmed = text.trim_end_matches('\n');
    assert!(
        !trimmed.is_empty(),
        "expected one line of JSON, got nothing"
    );
    assert!(
        !trimmed.contains('\n'),
        "expected a single line of JSON, got multiple: {text}"
    );
    serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("line is not valid JSON ({e}): {trimmed}"))
}

/// Stores one memory and returns the parsed response envelope.
fn remember(db: &Path, agent: &str, scope: &str, content: &str) -> Value {
    let assert = engram(db)
        .args(["remember", "--agent", agent, "--scope", scope, content])
        .assert()
        .success();
    parse_single_line_json(&assert.get_output().stdout)
}

/// Recalls a scope and returns the `data` array from the envelope.
fn recall_data(db: &Path, scope: &str) -> Vec<Value> {
    let assert = engram(db)
        .args(["recall", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    envelope["data"]
        .as_array()
        .expect("recall data is an array")
        .clone()
}

#[test]
fn remember_json_happy_path() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db)
        .args([
            "--json",
            "remember",
            "--agent",
            "claude-code",
            "--scope",
            "happy-path",
            "Decided: X stays synchronous.",
        ])
        .assert()
        .success();

    let envelope = parse_single_line_json(&assert.get_output().stdout);

    let metadata = &envelope["metadata"];
    assert_eq!(metadata["tool"], "engram");
    assert_eq!(metadata["command"], "engram memory remember");
    assert!(
        !metadata["version"]
            .as_str()
            .expect("version is a string")
            .is_empty(),
        "metadata.version must be non-empty"
    );

    let data = &envelope["data"];
    assert!(
        !data["id"].as_str().expect("data.id is a string").is_empty(),
        "data.id must be non-empty"
    );
    assert_eq!(data["agent"], "claude-code");
    assert_eq!(data["scope"], "happy-path");
    assert_eq!(data["role"], "note", "role defaults to note");
    assert_eq!(data["content"], "Decided: X stays synchronous.");
    let created_at = data["created_at"].as_str().expect("created_at is a string");
    assert!(
        created_at.ends_with('Z'),
        "created_at must be ISO 8601 UTC ending in Z, got {created_at}"
    );
}

#[test]
fn remember_reads_stdin_when_no_positional_content() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db)
        .args(["remember", "--agent", "kimi", "--scope", "stdin-scope"])
        .write_stdin("piped through stdin")
        .assert()
        .success();

    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["data"]["content"], "piped through stdin");
    assert_eq!(envelope["data"]["agent"], "kimi");

    // The write is real: recall sees it.
    let data = recall_data(&db, "stdin-scope");
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["content"], "piped through stdin");
}

#[test]
fn remember_without_content_or_stdin_is_invalid_argument() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db)
        .args(["remember", "--agent", "a", "--scope", "s"])
        .assert()
        .failure()
        .code(2);

    let err = parse_single_line_json(&assert.get_output().stderr);
    assert_eq!(err["error"]["code"], "INVALID_ARGUMENT");
    assert_eq!(err["error"]["exit_code"], 2);
    let hint = err["error"]["hint"]
        .as_str()
        .expect("error.hint is a string");
    assert!(!hint.is_empty(), "error.hint must be non-empty");
    let timestamp = err["error"]["timestamp"]
        .as_str()
        .expect("error.timestamp is a string");
    assert!(
        timestamp.ends_with('Z'),
        "error timestamp must be UTC, got {timestamp}"
    );
}

#[test]
fn remember_dry_run_validates_but_stores_nothing() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db)
        .args([
            "remember",
            "--agent",
            "claude-code",
            "--scope",
            "dry-scope",
            "--dry-run",
            "would be stored",
        ])
        .assert()
        .success();

    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["metadata"]["dry_run"], true);
    let actions = envelope["data"]["actions"]
        .as_array()
        .expect("dry-run data.actions is an array");
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["action"], "remember");
    assert_eq!(actions[0]["scope"], "dry-scope");
    assert_eq!(actions[0]["content"], "would be stored");

    // Nothing reached the database.
    let data = recall_data(&db, "dry-scope");
    assert!(
        data.is_empty(),
        "dry run must not store anything, got {data:?}"
    );
}

#[test]
fn recall_is_chronological_and_honors_limit() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "chronology";

    remember(&db, "a", scope, "first");
    remember(&db, "a", scope, "second");
    remember(&db, "a", scope, "third");

    let data = recall_data(&db, scope);
    let contents: Vec<&str> = data
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["first", "second", "third"]);

    let assert = engram(&db)
        .args(["recall", "--scope", scope, "--limit", "2"])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    let limited: Vec<&str> = envelope["data"]
        .as_array()
        .expect("recall data is an array")
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        limited,
        ["second", "third"],
        "--limit 2 keeps the most recent two, oldest first"
    );
}

#[test]
fn search_finds_matches_and_survives_operator_laden_queries() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "search-scope";

    remember(&db, "a", scope, "the reactor core is stable");
    remember(&db, "a", scope, "coolant pressure nominal");

    let assert = engram(&db)
        .args(["search", "reactor", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    let hits = envelope["data"]
        .as_array()
        .expect("search data is an array");
    assert_eq!(hits.len(), 1, "exactly one memory mentions the reactor");
    assert_eq!(hits[0]["content"], "the reactor core is stable");

    // FTS5 operator syntax must be neutralized by sanitization, not raise a
    // syntax error. "-x" needs the `--` escape so clap treats it as a
    // positional rather than a flag.
    for query in ["foo AND bar", "a:b", "core NEAR stable", "\"unbalanced"] {
        let assert = engram(&db).args(["search", query]).assert().success();
        let envelope = parse_single_line_json(&assert.get_output().stdout);
        assert!(
            envelope["data"].is_array(),
            "operator-laden query {query:?} must still yield a data array"
        );
    }
    let assert = engram(&db).args(["search", "--", "-x"]).assert().success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert!(envelope["data"].is_array());
}

#[test]
fn recall_format_jsonl_streams_metadata_then_records() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "jsonl-scope";

    remember(&db, "a", scope, "alpha");
    remember(&db, "a", scope, "beta");
    remember(&db, "a", scope, "gamma");

    let assert = engram(&db)
        .args(["--format", "jsonl", "recall", "--scope", scope])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 4, "one metadata line plus one line per memory");

    let head: Value = serde_json::from_str(lines[0]).expect("first jsonl line is JSON");
    assert!(
        head["metadata"].is_object(),
        "first line carries the metadata envelope"
    );
    assert!(head["data"].is_null(), "first line has data: null");

    let expected = ["alpha", "beta", "gamma"];
    for (line, want) in lines[1..].iter().zip(expected) {
        let record: Value = serde_json::from_str(line).expect("record line is JSON");
        assert!(
            record.is_object(),
            "each subsequent line is one JSON object"
        );
        assert_eq!(record["content"], want);
        assert_eq!(record["scope"], scope);
    }
}

#[test]
fn recall_format_csv_puts_rows_on_stdout_and_metadata_on_stderr() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "csv-scope";

    remember(&db, "a", scope, "row one");
    remember(&db, "a", scope, "row two");

    let assert = engram(&db)
        .args(["--format", "csv", "recall", "--scope", scope])
        .assert()
        .success();
    let output = assert.get_output();

    let stdout = String::from_utf8(output.stdout.clone()).expect("UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "header plus one row per memory");

    let header: Vec<&str> = lines[0].split(',').collect();
    assert!(
        header.contains(&"id"),
        "csv header names the id column: {}",
        lines[0]
    );
    assert!(
        header.contains(&"content"),
        "csv header names the content column: {}",
        lines[0]
    );

    let metadata_line = parse_single_line_json(&output.stderr);
    assert!(
        metadata_line["metadata"].is_object(),
        "stderr carries the metadata envelope as one JSON line"
    );
    assert_eq!(metadata_line["metadata"]["tool"], "engram");
}

#[test]
fn rule_add_list_retire_round_trip() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "rules-e2e";

    // Add.
    let assert = engram(&db)
        .args([
            "rule",
            "add",
            "--id",
            "no-unwrap",
            "--scope",
            scope,
            "--agent",
            "tester",
            "Library code must not call unwrap.",
        ])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["data"]["created"], true);
    assert_eq!(envelope["data"]["rule"]["rule_id"], "no-unwrap");
    assert_eq!(envelope["data"]["rule"]["scope"], scope);
    assert_eq!(envelope["data"]["scope_origin"], "explicit");

    // List shows it active.
    let assert = engram(&db)
        .args(["rule", "list", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["data"]["count"], 1);
    assert_eq!(envelope["data"]["rules"][0]["rule_id"], "no-unwrap");
    assert_eq!(envelope["data"]["rules"][0]["retired"], false);

    // Retire tombstones it.
    let assert = engram(&db)
        .args(["rule", "retire", "--id", "no-unwrap", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["data"]["outcome"], "retired");

    // Gone from the default listing.
    let assert = engram(&db)
        .args(["rule", "list", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["data"]["count"], 0);

    // Retiring an unknown id is a real error: exit 3, NOT_FOUND.
    let assert = engram(&db)
        .args(["rule", "retire", "--id", "no-such-rule", "--scope", scope])
        .assert()
        .failure()
        .code(3);
    let err = parse_single_line_json(&assert.get_output().stderr);
    assert_eq!(err["error"]["code"], "NOT_FOUND");
    assert_eq!(err["error"]["exit_code"], 3);
}

#[test]
fn rule_sync_dry_run_writes_no_files() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "sync-scope";

    engram(&db)
        .args([
            "rule",
            "add",
            "--id",
            "keep-it-simple",
            "--scope",
            scope,
            "--agent",
            "tester",
            "Prefer the simple design.",
        ])
        .assert()
        .success();

    // Run from the temp dir: it has no .git, so the project root — and the
    // default AGENTS.md/CLAUDE.md targets — resolve there.
    let assert = engram(&db)
        .current_dir(tmp.path())
        .args(["rule", "sync", "--scope", scope, "--dry-run"])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(
        envelope["metadata"]["dry_run"], true,
        "envelope contract is uniform with remember --dry-run"
    );
    assert_eq!(envelope["data"]["dry_run"], true);
    assert_eq!(envelope["data"]["rule_count"], 1);
    let files = envelope["data"]["files"]
        .as_array()
        .expect("data.files is an array");
    assert_eq!(
        files.len(),
        2,
        "default targets are AGENTS.md and CLAUDE.md"
    );
    for file in files {
        assert_eq!(file["dry_run"], true);
    }

    let missing = predicate::path::missing();
    assert!(
        missing.eval(&tmp.path().join("AGENTS.md")),
        "dry run must not create AGENTS.md"
    );
    assert!(
        missing.eval(&tmp.path().join("CLAUDE.md")),
        "dry run must not create CLAUDE.md"
    );
}

#[test]
fn recall_budget_tokens_drops_the_oldest_and_reports_it() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "budget-scope";

    // Each 4-char memory estimates to 1 token (ceil(chars/4)).
    remember(&db, "a", scope, "aaaa");
    remember(&db, "a", scope, "bbbb");
    remember(&db, "a", scope, "cccc");

    let assert = engram(&db)
        .args(["recall", "--scope", scope, "--budget-tokens", "2"])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);

    let contents: Vec<&str> = envelope["data"]
        .as_array()
        .expect("recall data is an array")
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        contents,
        ["bbbb", "cccc"],
        "a 2-token budget keeps the newest two, still chronological"
    );

    let budget = &envelope["metadata"]["budget"];
    assert_eq!(budget["estimator"], "chars-div-4");
    assert_eq!(budget["included"], 2);
    assert_eq!(budget["dropped"], 1);
    assert!(
        !budget["dropped_ids"]
            .as_array()
            .expect("dropped_ids is an array")
            .is_empty(),
        "the dropped memory is named"
    );

    // Without --budget-tokens the envelope is byte-identical to before:
    // no budget field at all.
    let assert = engram(&db)
        .args(["recall", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert!(
        envelope["metadata"]["budget"].is_null(),
        "no budget requested, so none serialized"
    );
}

#[test]
fn context_assembles_rules_then_memories_with_a_budget_report() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "context-scope";

    engram(&db)
        .args([
            "rule",
            "add",
            "--id",
            "context-first",
            "--scope",
            scope,
            "--agent",
            "tester",
            "Load context at session start.",
        ])
        .assert()
        .success();
    remember(&db, "a", scope, "the reactor stays at 80 percent");
    remember(&db, "a", scope, "coolant loop two is offline");

    let assert = engram(&db)
        .args(["context", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);

    assert_eq!(envelope["metadata"]["command"], "engram context");
    assert!(
        envelope["metadata"]["budget"].is_object(),
        "metadata carries the budget report"
    );

    let data = &envelope["data"];
    assert_eq!(data["scope"], scope);
    assert_eq!(data["scope_origin"], "explicit");
    let rules = data["rules"].as_array().expect("data.rules is an array");
    assert_eq!(rules.len(), 1, "the recorded rule leads the block");
    assert_eq!(rules[0]["rule_id"], "context-first");
    let memories = data["memories"]
        .as_array()
        .expect("data.memories is an array");
    assert_eq!(memories.len(), 2, "both memories fit the default budget");
    assert_eq!(
        memories[0]["content"], "the reactor stays at 80 percent",
        "memories are presented chronologically"
    );
    assert!(data["budget"].is_object(), "data carries the report too");
}

#[test]
fn schema_and_describe_are_valid_json() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db).arg("schema").assert().success();
    let schema = parse_json(&assert.get_output().stdout);
    assert!(
        schema["Memory"].is_object(),
        "schema exposes the Memory type"
    );
    assert!(schema["Rule"].is_object(), "schema exposes the Rule type");

    let assert = engram(&db).arg("describe").assert().success();
    let manifest = parse_json(&assert.get_output().stdout);
    assert_eq!(manifest["tool"], "engram");
    let formats = manifest["output"]["formats"]
        .as_array()
        .expect("describe .output.formats is an array");
    assert!(
        formats.iter().any(|f| f == "jsonl"),
        "describe must advertise jsonl, got {formats:?}"
    );
}

// --- M2: bi-temporal supersession ---------------------------------------

#[test]
fn supersede_round_trip_and_time_filters() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let out = engram(&db)
        .args(["remember", "--agent", "a", "--scope", "s", "old truth"])
        .assert()
        .success();
    let a_id = parse_single_line_json(&out.get_output().stdout)["data"]["id"]
        .as_str()
        .expect("id")
        .to_string();

    // Supersede it.
    let out = engram(&db)
        .args([
            "remember",
            "--agent",
            "b",
            "--scope",
            "s",
            "--supersedes",
            &a_id,
            "new truth",
        ])
        .assert()
        .success();
    let env = parse_single_line_json(&out.get_output().stdout);
    assert_eq!(env["data"]["outcome"], "superseded");
    assert_eq!(env["data"]["superseded_id"], a_id.as_str());
    let b_id = env["data"]["memory"]["id"]
        .as_str()
        .expect("new id")
        .to_string();

    // Default recall: only the replacement.
    let out = engram(&db)
        .args(["recall", "--scope", "s"])
        .assert()
        .success();
    let mems = parse_single_line_json(&out.get_output().stdout)["data"].clone();
    let ids: Vec<&str> = mems
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![b_id.as_str()]);

    // Full history via --include-superseded; the old row carries the chain.
    let out = engram(&db)
        .args(["recall", "--scope", "s", "--include-superseded"])
        .assert()
        .success();
    let mems = parse_single_line_json(&out.get_output().stdout)["data"].clone();
    assert_eq!(mems.as_array().unwrap().len(), 2);
    let old = mems
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == a_id.as_str())
        .expect("old row");
    assert_eq!(old["superseded_by"], b_id.as_str());
    assert!(old["valid_to"].is_string());

    // Double supersede: conflict, exit 5.
    let out = engram(&db)
        .args([
            "remember",
            "--agent",
            "c",
            "--scope",
            "s",
            "--supersedes",
            &a_id,
            "competing",
        ])
        .assert()
        .failure()
        .code(5);
    let err = parse_single_line_json(&out.get_output().stderr);
    assert_eq!(err["error"]["code"], "CONFLICT");
    assert_eq!(err["error"]["exit_code"], 5);

    // Unknown target: exit 3.
    let out = engram(&db)
        .args([
            "remember",
            "--agent",
            "c",
            "--scope",
            "s",
            "--supersedes",
            "nope",
            "x",
        ])
        .assert()
        .failure()
        .code(3);
    let err = parse_single_line_json(&out.get_output().stderr);
    assert_eq!(err["error"]["code"], "NOT_FOUND");
}

#[test]
fn as_of_validates_and_conflicts_with_include_superseded() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    // Malformed timestamp: structured exit 2.
    let out = engram(&db)
        .args(["recall", "--scope", "s", "--as-of", "yesterday"])
        .assert()
        .failure()
        .code(2);
    let err = parse_single_line_json(&out.get_output().stderr);
    assert_eq!(err["error"]["code"], "INVALID_ARGUMENT");

    // clap enforces mutual exclusion (usage error, exit 2).
    engram(&db)
        .args([
            "recall",
            "--scope",
            "s",
            "--as-of",
            "2026-08-01T00:00:00Z",
            "--include-superseded",
        ])
        .assert()
        .failure()
        .code(2);
}

// --- M3: vector feature gates -------------------------------------------

/// `engram index` always parses, and always refuses with a structured exit-2
/// error when it cannot run: in a default build because the vector feature
/// is compiled out, in a vector build because this hermetic environment
/// resolves no model. Either way the refusal is INVALID_ARGUMENT with a
/// non-empty hint — never a panic, never a silent success.
#[test]
fn index_without_a_usable_model_is_invalid_argument() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db).arg("index").assert().failure().code(2);
    let err = parse_single_line_json(&assert.get_output().stderr);
    assert_eq!(err["error"]["code"], "INVALID_ARGUMENT");
    let hint = err["error"]["hint"].as_str().expect("hint is a string");
    assert!(!hint.is_empty(), "the refusal explains the next step");
    if cfg!(feature = "vector") {
        assert!(
            hint.contains("--model-path") && hint.contains("ENGRAM_MODEL"),
            "the vector build explains the model cascade: {hint}"
        );
    } else {
        assert!(
            hint.contains("--features vector"),
            "the default build names the missing feature: {hint}"
        );
    }
}

/// `--mode fts` always works; `--mode hybrid` without its prerequisites is a
/// structured exit-2 error in every build (missing feature, missing model,
/// or nothing indexed — this hermetic environment guarantees at least one).
#[test]
fn search_mode_flag_gates_hybrid_and_keeps_fts_available() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    remember(&db, "a", "mode-scope", "the antenna is aligned");

    let assert = engram(&db)
        .args([
            "search",
            "antenna",
            "--scope",
            "mode-scope",
            "--mode",
            "fts",
        ])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["data"].as_array().map(Vec::len), Some(1));

    let assert = engram(&db)
        .args(["search", "antenna", "--mode", "hybrid"])
        .assert()
        .failure()
        .code(2);
    let err = parse_single_line_json(&assert.get_output().stderr);
    assert_eq!(err["error"]["code"], "INVALID_ARGUMENT");
    assert!(
        !err["error"]["hint"].as_str().expect("hint").is_empty(),
        "the refusal explains what is missing"
    );
}

/// The capability manifest advertises the index command and the vector
/// posture, including whether this binary was built with the feature.
#[test]
fn describe_advertises_index_and_the_vector_posture() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let assert = engram(&db).arg("describe").assert().success();
    let manifest = parse_json(&assert.get_output().stdout);
    let commands = manifest["commands"]
        .as_array()
        .expect("describe .commands is an array");
    assert!(
        commands.iter().any(|c| c == "index"),
        "describe must advertise index, got {commands:?}"
    );
    assert_eq!(
        manifest["vector"]["feature_enabled"],
        cfg!(feature = "vector"),
        "feature_enabled reflects the build"
    );
    assert!(
        manifest["vector"]["model_cascade"].is_array(),
        "the model cascade is machine-readable"
    );
}

/// Full local end-to-end: index a scope with a real Model2Vec model, then
/// search it in hybrid mode. Needs a model on disk, which CI runners and
/// most dev machines do not have — run explicitly with
/// `ENGRAM_TEST_MODEL=/path/to/model cargo test --features vector -- --ignored`.
#[cfg(feature = "vector")]
#[test]
#[ignore = "needs a local model2vec model (set ENGRAM_TEST_MODEL to its directory)"]
fn index_then_hybrid_search_with_a_real_model() {
    let model = std::env::var("ENGRAM_TEST_MODEL")
        .expect("set ENGRAM_TEST_MODEL to a Model2Vec model directory");
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "e2e-hybrid";

    remember(&db, "a", scope, "the launch window opens at dawn");
    remember(&db, "a", scope, "liftoff begins when the sun rises");

    let assert = engram(&db)
        .args(["--model-path", &model, "index", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["data"]["indexed"], 2);
    assert_eq!(envelope["data"]["remaining"], 0);
    assert!(envelope["data"]["dim"].as_u64().is_some_and(|d| d > 0));

    let assert = engram(&db)
        .args([
            "--model-path",
            &model,
            "search",
            "launch",
            "--scope",
            scope,
            "--mode",
            "hybrid",
        ])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    let hits = envelope["data"].as_array().expect("data is an array");
    assert!(
        !hits.is_empty(),
        "hybrid search returns results: {envelope}"
    );
}

#[test]
fn rule_purge_deletes_tombstones_only_and_needs_consent() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "purge-scope";

    engram(&db)
        .args([
            "rule",
            "add",
            "--id",
            "doomed",
            "--scope",
            scope,
            "Temporary policy.",
        ])
        .assert()
        .success();

    // Active rule: refused.
    engram(&db)
        .args(["rule", "purge", "--id", "doomed", "--scope", scope, "--yes"])
        .assert()
        .failure()
        .code(2);

    engram(&db)
        .args(["rule", "retire", "--id", "doomed", "--scope", scope])
        .assert()
        .success();

    // No --yes: refused with the full invocation in the hint.
    let out = engram(&db)
        .args(["rule", "purge", "--id", "doomed", "--scope", scope])
        .assert()
        .failure()
        .code(2);
    let err = parse_single_line_json(&out.get_output().stderr);
    assert!(err["error"]["hint"].as_str().unwrap().contains("--yes"));

    // Dry run: previewed, not deleted.
    let out = engram(&db)
        .args([
            "rule",
            "purge",
            "--id",
            "doomed",
            "--scope",
            scope,
            "--dry-run",
        ])
        .assert()
        .success();
    let env = parse_single_line_json(&out.get_output().stdout);
    assert_eq!(env["metadata"]["dry_run"], true);
    assert_eq!(env["data"]["actions"][0]["action"], "purge-rule");

    // Real purge.
    let out = engram(&db)
        .args(["rule", "purge", "--id", "doomed", "--scope", scope, "--yes"])
        .assert()
        .success();
    assert_eq!(
        parse_single_line_json(&out.get_output().stdout)["data"]["outcome"],
        "purged"
    );

    // Gone even from the tombstone view; unknown id now exits 3.
    let out = engram(&db)
        .args(["rule", "list", "--scope", scope, "--include-retired"])
        .assert()
        .success();
    assert_eq!(
        parse_single_line_json(&out.get_output().stdout)["data"]["count"],
        0
    );
    engram(&db)
        .args(["rule", "purge", "--id", "doomed", "--scope", scope, "--yes"])
        .assert()
        .failure()
        .code(3);
}

// --- M4: extracted-fact index --------------------------------------------

/// `consolidate` with no phase flag is a structured refusal, not a no-op:
/// at M4 only `--extract` exists, and the hint says what is coming.
#[test]
fn consolidate_without_extract_is_invalid_argument() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db).arg("consolidate").assert().failure().code(2);
    let err = parse_single_line_json(&assert.get_output().stderr);
    assert_eq!(err["error"]["code"], "INVALID_ARGUMENT");
    assert_eq!(err["error"]["exit_code"], 2);
    let hint = err["error"]["hint"].as_str().expect("hint is a string");
    assert!(
        hint.contains("--extract"),
        "the hint names the missing phase flag: {hint}"
    );
}

/// `--dry-run` reports the full extraction outcome without persisting it:
/// the subsequent real run writes exactly what the dry run predicted.
#[test]
fn consolidate_extract_dry_run_counts_without_writing() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "facts-dry";

    remember(
        &db,
        "a",
        scope,
        "Decided: the flange protocol stays synchronous.",
    );
    remember(&db, "a", scope, "plain chatter with no marker anywhere");

    let assert = engram(&db)
        .args(["consolidate", "--extract", "--scope", scope, "--dry-run"])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["metadata"]["dry_run"], true);
    assert_eq!(envelope["data"]["scanned"], 2);
    assert_eq!(envelope["data"]["memories_with_facts"], 1);
    assert_eq!(envelope["data"]["facts_written"], 1);

    // Nothing was persisted: the real run still writes the same count (a
    // fact already on disk would have been an upsert either way, but the
    // channel proves absence below too).
    let assert = engram(&db)
        .args(["consolidate", "--extract", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert!(
        envelope["metadata"]["dry_run"].is_null(),
        "no dry_run marker on a real run"
    );
    assert_eq!(envelope["data"]["facts_written"], 1);
}

/// Deterministic ids make re-extraction an upsert: the same report on every
/// run, and no growth in what retrieval sees.
#[test]
fn consolidate_extract_is_idempotent() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "facts-idem";

    remember(
        &db,
        "a",
        scope,
        "Decided: use the zeta coupling.\nTODO inspect the zeta coupling weld",
    );
    remember(&db, "a", scope, "no markers in this one");

    let run = |db: &Path| -> Value {
        let assert = engram(db)
            .args(["consolidate", "--extract", "--scope", scope])
            .assert()
            .success();
        parse_single_line_json(&assert.get_output().stdout)["data"].clone()
    };
    let first = run(&db);
    assert_eq!(first["scanned"], 2);
    assert_eq!(first["memories_with_facts"], 1);
    assert_eq!(first["facts_written"], 2);

    let second = run(&db);
    assert_eq!(
        second["facts_written"], first["facts_written"],
        "re-extraction rewrites the same rows, it does not accumulate"
    );
    assert_eq!(second["scanned"], first["scanned"]);
}

/// After extraction, a `context --query` in marker phrasing surfaces the
/// decision memory and reports the facts channel in the budget metadata.
#[test]
fn context_query_surfaces_the_facts_channel_after_extraction() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "facts-ctx";

    remember(
        &db,
        "a",
        scope,
        "Decided: adopt the flange protocol for docking.",
    );
    remember(&db, "a", scope, "ordinary chatter about the weather");

    // Before extraction the channel exists but is empty.
    let assert = engram(&db)
        .args(["context", "--scope", scope, "--query", "flange protocol"])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["metadata"]["budget"]["channels"]["facts"], 0);

    engram(&db)
        .args(["consolidate", "--extract", "--scope", scope])
        .assert()
        .success();

    let assert = engram(&db)
        .args(["context", "--scope", scope, "--query", "flange protocol"])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(
        envelope["metadata"]["budget"]["channels"]["facts"], 1,
        "the extracted fact's parent is a facts-channel candidate"
    );
    let memories = envelope["data"]["memories"]
        .as_array()
        .expect("data.memories is an array");
    assert!(
        memories
            .iter()
            .any(|m| m["content"] == "Decided: adopt the flange protocol for docking."),
        "the decision memory is in the assembled context: {memories:?}"
    );
}
