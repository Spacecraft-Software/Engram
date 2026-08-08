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
const HERMETIC_VARS: [&str; 15] = [
    "AI_AGENT",
    "AGENT",
    "CI",
    // Every harness path derives from `harness::home_dir`, which reads $HOME
    // and nothing else. Scrubbing it here means a test that does not set a
    // fake HOME cannot accidentally read the developer's real transcripts.
    "HOME",
    // `save-chat --model` falls back to these; an unscrubbed MODEL on the
    // developer's machine would leak into `signed_by`.
    "MODEL",
    "LLM_MODEL",
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
    // default AGENTS.md target — resolves there.
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
        1,
        "AGENTS.md is the sole default target (Standard §5.7); CLAUDE.md \
         receives the block through its @AGENTS.md import, so writing both \
         would deliver it twice and create two copies that can disagree"
    );
    assert_eq!(
        files[0]["path"].as_str().expect("file path is a string"),
        tmp.path().join("AGENTS.md").to_string_lossy(),
        "the default target is AGENTS.md, not CLAUDE.md"
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
        "CLAUDE.md is not a sync target at all (§5.7), dry run or not"
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
/// the hint names every available phase.
#[test]
fn consolidate_without_a_phase_flag_is_invalid_argument() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db).arg("consolidate").assert().failure().code(2);
    let err = parse_single_line_json(&assert.get_output().stderr);
    assert_eq!(err["error"]["code"], "INVALID_ARGUMENT");
    assert_eq!(err["error"]["exit_code"], 2);
    let hint = err["error"]["hint"].as_str().expect("hint is a string");
    for phase in ["--extract", "--dedup", "--report"] {
        assert!(hint.contains(phase), "the hint names phase {phase}: {hint}");
    }
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
    let extract = &envelope["data"]["extract"];
    assert_eq!(extract["scanned"], 2);
    assert_eq!(extract["memories_with_facts"], 1);
    assert_eq!(extract["facts_written"], 1);
    assert!(
        envelope["data"]["dedup"].is_null() && envelope["data"]["report"].is_null(),
        "phases that did not run are absent from the data"
    );

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
    assert_eq!(envelope["data"]["extract"]["facts_written"], 1);
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
        parse_single_line_json(&assert.get_output().stdout)["data"]["extract"].clone()
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

// --- M5: idle consolidation + decay ---------------------------------------

/// `--dedup` without `--yes` names the duplicate group but changes nothing;
/// `--dedup --yes` supersedes the older copies (never deletes), and a second
/// run finds nothing — the losers are no longer Current.
#[test]
fn consolidate_dedup_reports_then_supersedes_with_yes_idempotently() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "dedup-cli";

    // Two copies distinct only in case/whitespace, plus a bystander.
    let older = remember(
        &db,
        "a",
        scope,
        "Decided: torque stays at five newton meters.",
    );
    let older_id = older["data"]["id"].as_str().expect("id").to_string();
    let newer = remember(
        &db,
        "a",
        scope,
        "  decided:   TORQUE stays at five newton meters.  ",
    );
    let newer_id = newer["data"]["id"].as_str().expect("id").to_string();
    remember(&db, "a", scope, "unrelated bystander memory");

    // Report-only: the group is named; every row stays Current.
    let assert = engram(&db)
        .args(["consolidate", "--dedup", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    let dedup = &envelope["data"]["dedup"];
    assert_eq!(dedup["applied"], false);
    assert_eq!(dedup["superseded"], 0);
    let groups = dedup["groups"].as_array().expect("groups is an array");
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0]["winner"],
        newer_id.as_str(),
        "the newest row wins"
    );
    assert_eq!(groups[0]["losers"][0], older_id.as_str());
    assert_eq!(
        recall_data(&db, scope).len(),
        3,
        "report-only leaves every row Current"
    );

    // --yes: the loser is superseded by the winner. Never deleted.
    let assert = engram(&db)
        .args(["consolidate", "--dedup", "--scope", scope, "--yes"])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(envelope["data"]["dedup"]["applied"], true);
    assert_eq!(envelope["data"]["dedup"]["superseded"], 1);
    let current = recall_data(&db, scope);
    assert_eq!(current.len(), 2, "the loser left the Current view");
    assert!(current.iter().all(|m| m["id"] != older_id.as_str()));

    // The full history keeps the loser, chained to the winner.
    let assert = engram(&db)
        .args(["recall", "--scope", scope, "--include-superseded"])
        .assert()
        .success();
    let all = parse_single_line_json(&assert.get_output().stdout)["data"].clone();
    let loser_row = all
        .as_array()
        .expect("array")
        .iter()
        .find(|m| m["id"] == older_id.as_str())
        .expect("the superseded row survives")
        .clone();
    assert_eq!(loser_row["superseded_by"], newer_id.as_str());
    assert!(loser_row["valid_to"].is_string());

    // Idempotent: nothing left to find.
    let assert = engram(&db)
        .args(["consolidate", "--dedup", "--scope", scope, "--yes"])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(
        envelope["data"]["dedup"]["groups"].as_array().map(Vec::len),
        Some(0),
        "a second run finds nothing"
    );
    assert_eq!(envelope["data"]["dedup"]["superseded"], 0);
}

/// `--report` returns both sections — contradiction pairs and decay
/// candidates — and is always read-only.
#[test]
fn consolidate_report_returns_contradictions_and_decay() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "report-cli";

    remember(
        &db,
        "a",
        scope,
        "the deploy pipeline uses docker for builds",
    );
    let negated = remember(
        &db,
        "a",
        scope,
        "the deploy pipeline does not use docker for builds",
    );
    let negated_id = negated["data"]["id"].as_str().expect("id");

    let assert = engram(&db)
        .args(["consolidate", "--report", "--scope", scope])
        .assert()
        .success();
    let envelope = parse_single_line_json(&assert.get_output().stdout);
    let report = &envelope["data"]["report"];

    let contradictions = report["contradictions"]
        .as_array()
        .expect("contradictions is an array");
    assert_eq!(contradictions.len(), 1, "{contradictions:?}");
    assert_eq!(contradictions[0]["negated"], negated_id);
    assert!(contradictions[0]["jaccard"].as_f64().expect("jaccard") >= 0.5);

    let decay = report["decay"].as_array().expect("decay is an array");
    assert_eq!(decay.len(), 2, "both memories are scored");
    for candidate in decay {
        assert!(candidate["staleness"].is_number());
        assert!(candidate["access_count"].is_number());
        assert!(candidate["age_days"].is_number());
    }
    // The report itself is read-only: both rows are still Current.
    assert_eq!(recall_data(&db, scope).len(), 2);
}

/// `--no-track` keeps reads from bumping access counters; a tracked read
/// bumps them — observable through the decay report's `access_count`. The
/// serialized memories never carry the access columns either way.
#[test]
fn no_track_recall_leaves_access_counts_untouched() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let scope = "no-track-cli";

    remember(&db, "a", scope, "the audited memory");

    // Audited read: no bump.
    let assert = engram(&db)
        .args(["--no-track", "recall", "--scope", scope])
        .assert()
        .success();
    let data = parse_single_line_json(&assert.get_output().stdout)["data"].clone();
    let row = &data.as_array().expect("array")[0];
    assert!(
        row.get("access_count").is_none() && row.get("last_accessed_at").is_none(),
        "access columns are internal and never serialized: {row}"
    );

    let access_count = |db: &Path| -> i64 {
        let assert = engram(db)
            .args(["consolidate", "--report", "--scope", scope])
            .assert()
            .success();
        parse_single_line_json(&assert.get_output().stdout)["data"]["report"]["decay"][0]
            ["access_count"]
            .as_i64()
            .expect("access_count")
    };
    assert_eq!(access_count(&db), 0, "--no-track left the counter at 0");

    // Tracked read: bumps once. (consolidate --report itself never tracks —
    // it is analysis, not retrieval — so the counter is stable across the
    // probe calls.)
    engram(&db)
        .args(["recall", "--scope", scope])
        .assert()
        .success();
    assert_eq!(access_count(&db), 1, "a tracked recall bumps once");
}

/// The capability manifest advertises the consolidate phases and the
/// access-tracking posture.
#[test]
fn describe_advertises_consolidate_phases_and_access_tracking() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let assert = engram(&db).arg("describe").assert().success();
    let manifest = parse_json(&assert.get_output().stdout);

    let phases = manifest["consolidate"]["phases"]
        .as_array()
        .expect("consolidate.phases is an array");
    for phase in ["extract", "dedup", "report"] {
        assert!(
            phases.iter().any(|p| p == phase),
            "phases must include {phase}: {phases:?}"
        );
    }
    assert_eq!(manifest["consolidate"]["cli_only"], true);

    let tracking = &manifest["access_tracking"];
    let columns = tracking["columns"]
        .as_array()
        .expect("access_tracking.columns is an array");
    assert!(columns.iter().any(|c| c == "access_count"));
    assert!(columns.iter().any(|c| c == "last_accessed_at"));
    assert_eq!(tracking["opt_out"], "--no-track (CLI)");
}

// ---------------------------------------------------------------- save-chat

/// A `save-chat` invocation rooted at `project`, so scope resolution walks up
/// from a scratch directory instead of the crate's own git tree — the command
/// writes `chat/` and `.gitignore` at whatever root it resolves.
fn save_chat(db: &Path, project: &Path, args: &[&str]) -> Command {
    let mut cmd = engram(db);
    cmd.current_dir(project);
    cmd.arg("save-chat");
    cmd.args(args);
    cmd
}

/// The defect this rewrite exists to fix: `save-chat` used to append by
/// stripping the trailing `@bye` and re-emitting the entire history, so every
/// re-run duplicated every message and grew the file without bound.
#[test]
fn save_chat_twice_is_byte_identical() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");
    let scope = "archive-idempotent";

    remember(&db, "a", scope, "the first message");
    remember(&db, "b", scope, "the second message");

    let first = save_chat(
        &db,
        &project,
        &[
            "--scope",
            scope,
            "--file",
            "archive.texi",
            "--model",
            "opus-5",
        ],
    )
    .assert()
    .success();
    let first_json = parse_single_line_json(&first.get_output().stdout);
    assert_eq!(first_json["data"]["file"]["outcome"], "created");
    assert_eq!(first_json["data"]["messages_saved"], 2);
    assert_eq!(first_json["metadata"]["command"], "engram save-chat");

    let path = project.join("archive.texi");
    let after_first = std::fs::read(&path).expect("archive written");

    let second = save_chat(
        &db,
        &project,
        &[
            "--scope",
            scope,
            "--file",
            "archive.texi",
            "--model",
            "opus-5",
        ],
    )
    .assert()
    .success();
    let second_json = parse_single_line_json(&second.get_output().stdout);
    assert_eq!(
        second_json["data"]["file"]["outcome"], "unchanged",
        "an unchanged scope must not rewrite the archive"
    );

    let after_second = std::fs::read(&path).expect("archive still there");
    assert_eq!(
        after_first, after_second,
        "re-running save-chat must be byte-identical"
    );

    let text = String::from_utf8(after_second).expect("archive is UTF-8");
    assert_eq!(
        text.matches("the first message").count(),
        1,
        "message duplicated across runs: {text}"
    );
    assert_eq!(text.matches("@bye").count(), 1);
    assert!(text.contains("@documentencoding UTF-8"));
}

/// An archive is a transcript. Rules travel through `rule sync`, and used to
/// leak in because `save-chat` read through `recall`, which returns them.
#[test]
fn save_chat_excludes_rule_rows() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");
    let scope = "archive-rules";

    remember(&db, "a", scope, "an ordinary message");
    engram(&db)
        .args([
            "rule",
            "add",
            "--id",
            "never-archive-me",
            "--scope",
            scope,
            "--agent",
            "test",
            "this rule text must not reach the archive",
        ])
        .assert()
        .success();

    let assert = save_chat(&db, &project, &["--scope", scope, "--file", "archive.texi"])
        .assert()
        .success();
    assert_eq!(
        parse_single_line_json(&assert.get_output().stdout)["data"]["messages_saved"],
        1
    );

    let text = std::fs::read_to_string(project.join("archive.texi")).expect("archive written");
    assert!(text.contains("an ordinary message"));
    assert!(
        !text.contains("this rule text must not reach the archive"),
        "rule row leaked into the archive: {text}"
    );
}

/// Archiving is not reading. Exporting through `recall` used to bump every
/// exported row's counter, making the whole scope look freshly used to the
/// decay report.
#[test]
fn save_chat_does_not_bump_access_counts() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");
    let scope = "archive-untracked";

    remember(&db, "a", scope, "the archived memory");

    let access_count = || -> i64 {
        let assert = engram(&db)
            .args(["consolidate", "--report", "--scope", scope])
            .assert()
            .success();
        parse_single_line_json(&assert.get_output().stdout)["data"]["report"]["decay"][0]
            ["access_count"]
            .as_i64()
            .expect("access_count")
    };
    assert_eq!(access_count(), 0);

    save_chat(&db, &project, &["--scope", scope, "--file", "archive.texi"])
        .assert()
        .success();

    assert_eq!(
        access_count(),
        0,
        "exporting must not touch the decay signal"
    );
}

#[test]
fn save_chat_dry_run_writes_no_files() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");
    let scope = "archive-dry-run";

    remember(&db, "a", scope, "a message");

    let assert = save_chat(
        &db,
        &project,
        &["--scope", scope, "--file", "archive.texi", "--dry-run"],
    )
    .assert()
    .success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(json["data"]["file"]["outcome"], "created");
    assert_eq!(json["data"]["file"]["dry_run"], true);
    assert_eq!(json["metadata"]["dry_run"], true);

    assert!(predicate::path::missing().eval(&project.join("archive.texi")));
    assert!(predicate::path::missing().eval(&project.join(".gitignore")));
}

/// `--file` is relative to the resolved project root, not the process cwd, so
/// the same command from a subdirectory targets the same file.
#[test]
fn save_chat_resolves_relative_file_against_the_project_root() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let project = tmp.path().join("project");
    let nested = project.join("deep").join("nested");
    std::fs::create_dir_all(&nested).expect("create nested dir");
    // A `.git` marker makes `project` the resolved root from anywhere below.
    std::fs::write(project.join(".git"), "gitdir: elsewhere\n").expect("write .git marker");
    let scope = "archive-root";

    remember(&db, "a", scope, "a message");

    let assert = save_chat(&db, &nested, &["--scope", scope, "--file", "archive.texi"])
        .assert()
        .success();
    let json = parse_single_line_json(&assert.get_output().stdout);

    assert_eq!(
        json["data"]["root"].as_str().expect("root"),
        project.to_string_lossy()
    );
    assert!(
        project.join("archive.texi").exists(),
        "archive must land at the project root, not the cwd"
    );
    assert!(predicate::path::missing().eval(&nested.join("archive.texi")));

    // `chat/` is gitignored at the root, once, and reported rather than
    // written silently. The report names engram as the actor and the file as
    // the object: a bare `gitignore_updated: false` read as a failure.
    let gi = &json["data"]["gitignore"];
    assert_eq!(gi["action"], "added");
    assert_eq!(gi["entry"], "chat/");
    assert_eq!(
        gi["path"].as_str().expect("path"),
        project.join(".gitignore").to_string_lossy()
    );
    let detail = gi["detail"].as_str().expect("detail");
    assert!(detail.starts_with("engram "), "{detail}");
    assert!(detail.contains(".gitignore"), "{detail}");
    let ignored = std::fs::read_to_string(project.join(".gitignore")).expect("gitignore written");
    assert_eq!(ignored.matches("chat/").count(), 1);
}

/// The three `.gitignore` outcomes are distinguishable, and only one of them
/// writes. The replaced boolean reported `true` for a dry run, claiming an
/// update that never happened, and `false` for "already ignored", which read
/// as a failure rather than as the no-op it is.
#[test]
fn save_chat_reports_each_gitignore_outcome_distinctly() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project");
    std::fs::write(project.join(".git"), "gitdir: elsewhere\n").expect("write .git marker");
    let scope = "gitignore-outcomes";
    remember(&db, "a", scope, "a message");

    let action = |args: &[&str]| -> (String, String) {
        let assert = save_chat(&db, &project, args).assert().success();
        let json = parse_single_line_json(&assert.get_output().stdout);
        let gi = &json["data"]["gitignore"];
        (
            gi["action"].as_str().expect("action").to_string(),
            gi["detail"].as_str().expect("detail").to_string(),
        )
    };

    // Dry run: reported as would-add, and nothing is written.
    let (act, detail) = action(&["--scope", scope, "--file", "a.texi", "--dry-run"]);
    assert_eq!(act, "would-add");
    assert!(detail.contains("dry run"), "{detail}");
    assert!(predicate::path::missing().eval(&project.join(".gitignore")));

    // First real run: added.
    let (act, detail) = action(&["--scope", scope, "--file", "a.texi"]);
    assert_eq!(act, "added");
    assert!(detail.starts_with("engram added"), "{detail}");

    // Second run: already ignored, no second entry.
    let (act, detail) = action(&["--scope", scope, "--file", "a.texi"]);
    assert_eq!(act, "already-ignored");
    assert!(detail.contains("already ignored"), "{detail}");
    let ignored = std::fs::read_to_string(project.join(".gitignore")).expect("read");
    assert_eq!(ignored.matches("chat/").count(), 1);
}

/// The archive must be a *valid* Texinfo document, not merely Texinfo-shaped.
/// Skipped when `makeinfo` is not installed; the render-level invariants are
/// covered unconditionally by the unit tests in `main.rs`.
#[test]
fn save_chat_output_compiles_under_makeinfo() {
    let Ok(makeinfo) = which_makeinfo() else {
        eprintln!("skipping: makeinfo not on PATH");
        return;
    };

    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project dir");
    let scope = "archive-makeinfo";

    // Exercise the escaping and the encoding declaration together: markup
    // characters and non-ASCII are exactly what a real transcript carries.
    remember(&db, "a", scope, "braces {here}, an @ sign, café, 日本語");
    remember(&db, "b", scope, "a second message");

    save_chat(&db, &project, &["--scope", scope, "--file", "archive.texi"])
        .assert()
        .success();

    let out = std::process::Command::new(makeinfo)
        .arg("--no-split")
        .arg("-o")
        .arg(project.join("archive.info"))
        .arg(project.join("archive.texi"))
        .output()
        .expect("run makeinfo");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "makeinfo rejected the archive:\n{stderr}"
    );
    assert!(
        stderr.trim().is_empty(),
        "makeinfo emitted warnings:\n{stderr}"
    );
}

fn which_makeinfo() -> Result<std::path::PathBuf, ()> {
    let path = std::env::var_os("PATH").ok_or(())?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("makeinfo"))
        .find(|candidate| candidate.is_file())
        .ok_or(())
}

// ------------------------------------------------------------------- ingest

/// Plants the synthetic Claude Code fixture where the reader will look for a
/// session belonging to `project`, inside a fake `$HOME`.
///
/// The directory name is produced by mangling `project` forward — `/` becomes
/// `-`, case preserved — exactly as the reader does. Nothing here reverses a
/// mangled name, because that mapping does not exist.
fn plant_claude_transcript(home: &Path, project: &Path, session_id: &str) -> std::path::PathBuf {
    let mangled = project.to_string_lossy().replace('/', "-");
    let dir = home.join(".claude").join("projects").join(mangled);
    std::fs::create_dir_all(&dir).expect("create fake projects dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts/claude-code/session-basic.jsonl");
    let dest = dir.join(format!("{session_id}.jsonl"));
    std::fs::copy(&src, &dest).expect("copy fixture into the fake home");
    dest
}

/// An `engram ingest` invocation against a fake `$HOME` and project root.
fn ingest(db: &Path, home: &Path, project: &Path, args: &[&str]) -> Command {
    let mut cmd = engram(db);
    cmd.env("HOME", home);
    cmd.current_dir(project);
    cmd.arg("ingest");
    cmd.args(args);
    cmd
}

/// Builds a fake home + project pair inside one tempdir.
fn ingest_fixture(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = tmp.path().join("home");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&home).expect("create fake home");
    std::fs::create_dir_all(&project).expect("create project");
    (home, project)
}

#[test]
fn ingest_dry_run_reports_the_filter_histogram_and_writes_nothing() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_claude_transcript(&home, &project, "session-basic");

    let assert = ingest(
        &db,
        &home,
        &project,
        &["--harness", "claude-code", "--scope", "ing", "--dry-run"],
    )
    .assert()
    .success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    let data = &json["data"];

    assert_eq!(json["metadata"]["command"], "engram ingest");
    assert_eq!(json["metadata"]["dry_run"], true);
    assert_eq!(data["harness"], "claude-code");

    // Four real turns survive: two user messages and two assistant replies.
    // Everything else in the fixture is machinery.
    assert_eq!(data["inserted"], 4, "unexpected turn count: {data}");

    let filtered = &data["filtered"];
    assert_eq!(filtered["thinking"], 1);
    assert_eq!(filtered["tool_use"], 1);
    assert_eq!(filtered["tool_result"], 1);
    assert_eq!(filtered["meta"], 1);
    assert_eq!(filtered["sidechain"], 1);
    assert_eq!(filtered["command_synthetic"], 2);
    // The early-warning signal for a transcript-format change.
    assert_eq!(filtered["unknown_record"], 1);

    // Nothing was stored.
    let assert = engram(&db)
        .args(["recall", "--scope", "ing"])
        .assert()
        .success();
    let rows = parse_single_line_json(&assert.get_output().stdout)["data"]
        .as_array()
        .expect("array")
        .len();
    assert_eq!(rows, 0, "--dry-run must not write");
}

#[test]
fn ingest_stores_turns_and_is_idempotent() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_claude_transcript(&home, &project, "session-basic");

    let args = ["--harness", "claude-code", "--scope", "ing"];
    let assert = ingest(&db, &home, &project, &args).assert().success();
    let first = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(first["data"]["inserted"], 4);
    assert_eq!(first["data"]["skipped_existing"], 0);

    // Re-ingesting the same session inserts nothing: the id is a pure
    // function of (harness, session, record).
    let assert = ingest(&db, &home, &project, &args).assert().success();
    let second = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(second["data"]["inserted"], 0, "re-ingest must be a no-op");
    assert_eq!(second["data"]["skipped_existing"], 4);

    let rows = recall_data(&db, "ing");
    assert_eq!(rows.len(), 4, "still four rows after two ingests");

    // Roles the schema has always declared and nothing ever wrote until now.
    let roles: Vec<&str> = rows
        .iter()
        .map(|r| r["role"].as_str().expect("role"))
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert!(rows.iter().all(|r| r["agent"] == "claude-code"));

    // Chronological, from the transcript's own timestamps — not the wall
    // clock, which would collapse the conversation into one instant.
    let times: Vec<&str> = rows
        .iter()
        .map(|r| r["created_at"].as_str().expect("created_at"))
        .collect();
    assert_eq!(times[0], "2026-08-01T10:01:00Z");
    assert_eq!(times[3], "2026-08-01T10:05:00Z");
    let mut sorted = times.clone();
    sorted.sort_unstable();
    assert_eq!(times, sorted, "turns must be stored in reading order");
}

/// The two exclusions that make ingest safe to point at a shared database.
#[test]
fn ingest_excludes_tool_payloads_and_redacts_credentials() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_claude_transcript(&home, &project, "session-basic");

    let assert = ingest(
        &db,
        &home,
        &project,
        &["--harness", "claude-code", "--scope", "ing"],
    )
    .assert()
    .success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(json["data"]["redactions"]["github-token"], 1);

    let all: String = recall_data(&db, "ing")
        .iter()
        .map(|r| r["content"].as_str().expect("content").to_string())
        .collect::<Vec<_>>()
        .join("\n");

    // Tool payloads never enter: that is where file contents and command
    // output live.
    assert!(!all.contains("hunter2"), "tool payload leaked: {all}");
    assert!(
        !all.contains("AWS_SECRET_ACCESS_KEY"),
        "tool payload leaked: {all}"
    );
    // Thinking never enters by default.
    assert!(
        !all.contains("internal deliberation"),
        "thinking leaked: {all}"
    );
    // Credentials in prose are replaced.
    assert!(!all.contains("ghp_AAAA"), "credential leaked: {all}");
    assert!(all.contains("[redacted:github-token]"));
    // Injected harness context is not a participant's words.
    assert!(
        !all.contains("injected context"),
        "system-reminder leaked: {all}"
    );
    // ANSI escapes are stripped but the surrounding words survive.
    assert!(
        all.contains("Should the reader stream line by line?"),
        "got: {all}"
    );
    // Real prose is intact.
    assert!(all.contains("Decided: stream the reader and cap it with --max-bytes."));
}

/// Ingested turns are ordinary memories, so the rest of engram sees them.
#[test]
fn ingested_turns_are_searchable_and_archivable() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_claude_transcript(&home, &project, "session-basic");

    ingest(
        &db,
        &home,
        &project,
        &["--harness", "claude-code", "--scope", "ing"],
    )
    .assert()
    .success();

    let assert = engram(&db)
        .args(["search", "append-only", "--scope", "ing"])
        .assert()
        .success();
    let hits = parse_single_line_json(&assert.get_output().stdout)["data"]
        .as_array()
        .expect("array")
        .len();
    assert!(hits >= 1, "ingested turns must be full-text searchable");

    // And the full round trip the whole subsystem exists for.
    let assert = save_chat(&db, &project, &["--scope", "ing", "--file", "archive.texi"])
        .assert()
        .success();
    assert_eq!(
        parse_single_line_json(&assert.get_output().stdout)["data"]["messages_saved"],
        4
    );
    let archive = std::fs::read_to_string(project.join("archive.texi")).expect("archive");
    assert!(archive.contains("Should the reader stream line by line?"));
    assert!(!archive.contains("internal deliberation"));
}

/// The contract that must never regress: a harness engram cannot read is an
/// error, never an empty success. Asserted partly as an *absence* — stdout
/// must be empty, because a `{"data": {"sessions": []}}` on stdout would read
/// to a caller as "there is nothing here".
#[test]
fn ingest_from_a_readerless_harness_is_exit_2_with_a_fallback() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);

    for harness in ["antigravity", "copilot-cli", "goose", "qwen"] {
        let assert = ingest(&db, &home, &project, &["--harness", harness, "--list"])
            .assert()
            .failure()
            .code(2);

        assert!(
            assert.get_output().stdout.is_empty(),
            "{harness}: an unreadable harness must produce no stdout at all"
        );
        let err = parse_single_line_json(&assert.get_output().stderr);
        assert_eq!(err["error"]["code"], "INVALID_ARGUMENT");
        let message = err["error"]["message"].as_str().expect("message");
        assert!(message.contains(harness), "{harness}: {message}");
        // The hint must name the way forward, not just the refusal.
        let hint = err["error"]["hint"].as_str().expect("hint");
        assert!(
            hint.contains("remember"),
            "{harness} hint lacks a fallback: {hint}"
        );
        assert!(
            hint.contains("save-chat"),
            "{harness} hint lacks a fallback: {hint}"
        );
    }
}

#[test]
fn ingest_list_reports_sessions_and_the_harness_table_without_writing() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_claude_transcript(&home, &project, "session-basic");

    let assert = ingest(
        &db,
        &home,
        &project,
        &["--harness", "claude-code", "--list"],
    )
    .assert()
    .success();
    let data = parse_single_line_json(&assert.get_output().stdout)["data"].clone();

    let sessions = data["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], "session-basic");
    assert!(sessions[0]["bytes"].as_u64().expect("bytes") > 0);

    // The table lets a user tell "no sessions here" from "cannot read this
    // harness" without a second command.
    let harnesses = data["harnesses"].as_array().expect("harnesses array");
    let unreadable: Vec<_> = harnesses.iter().filter(|h| h["reader"].is_null()).collect();
    assert!(!unreadable.is_empty());
    for h in unreadable {
        assert!(
            h["reader_detail"].as_str().is_some_and(|d| !d.is_empty()),
            "a harness without a reader must say why: {h}"
        );
    }
}

#[test]
fn ingest_of_an_unknown_session_id_is_not_found() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_claude_transcript(&home, &project, "session-basic");

    let assert = ingest(
        &db,
        &home,
        &project,
        &["--harness", "claude-code", "--session", "no-such-session"],
    )
    .assert()
    .failure()
    .code(3);
    let err = parse_single_line_json(&assert.get_output().stderr);
    assert_eq!(err["error"]["code"], "NOT_FOUND");
}

#[test]
fn ingest_include_flags_widen_what_is_captured() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_claude_transcript(&home, &project, "session-basic");

    let assert = ingest(
        &db,
        &home,
        &project,
        &[
            "--harness",
            "claude-code",
            "--scope",
            "ing",
            "--include-thinking",
            "--include-tools",
            "--include-sidechains",
        ],
    )
    .assert()
    .success();
    let inserted = parse_single_line_json(&assert.get_output().stdout)["data"]["inserted"]
        .as_u64()
        .expect("inserted");
    assert!(inserted > 4, "opting in must capture more, got {inserted}");

    let all: String = recall_data(&db, "ing")
        .iter()
        .map(|r| r["content"].as_str().expect("content").to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all.contains("internal deliberation"),
        "thinking was requested"
    );
    assert!(
        all.contains("[tool_use: Bash]"),
        "tool calls were requested"
    );
    // Even opted in, the payload is summarized to a size — never inlined.
    assert!(all.contains("[tool_result:"));
    assert!(
        !all.contains("hunter2"),
        "payload leaked even with --include-tools: {all}"
    );
}

// ------------------------------------------------------------------ install

/// An `engram install` invocation against a fake `$HOME`.
fn install(db: &Path, home: &Path, args: &[&str]) -> Command {
    let mut cmd = engram(db);
    cmd.env("HOME", home);
    cmd.arg("install");
    cmd.args(args);
    cmd
}

/// Marks a harness as installed by creating the path engram probes for.
fn pretend_installed(home: &Path, relative: &str) {
    let path = home.join(relative);
    std::fs::create_dir_all(&path).expect("create fake harness home");
}

#[test]
fn install_list_reports_every_harness_and_writes_nothing() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create fake home");
    pretend_installed(&home, ".claude");

    let assert = install(&db, &home, &["--list"]).assert().success();
    let data = parse_single_line_json(&assert.get_output().stdout)["data"].clone();

    let harnesses = data["harnesses"].as_array().expect("harnesses");
    assert_eq!(harnesses.len(), 7, "every known harness must be reported");

    let claude = harnesses
        .iter()
        .find(|h| h["name"] == "claude-code")
        .expect("claude-code listed");
    assert_eq!(claude["present"], true);
    assert!(claude["commands_dir"]
        .as_str()
        .expect("commands_dir")
        .ends_with(".claude/commands"));

    // Harnesses without a command surface say so with a null rather than
    // being quietly omitted.
    let antigravity = harnesses
        .iter()
        .find(|h| h["name"] == "antigravity")
        .expect("antigravity listed");
    assert!(antigravity["commands_dir"].is_null());

    // Nothing was created, not even for the harness that is "installed".
    assert!(predicate::path::missing().eval(&home.join(".claude/commands")));
}

#[test]
fn install_writes_commands_and_is_idempotent() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create fake home");
    pretend_installed(&home, ".claude");

    let assert = install(&db, &home, &["--db-path", "/shared/engram.db"])
        .assert()
        .success();
    let first = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(first["metadata"]["command"], "engram install");
    assert_eq!(first["data"]["installed"], 3);

    let dir = home.join(".claude/commands");
    for name in ["save-chat", "ingest", "context"] {
        let path = dir.join(format!("engram-{name}.md"));
        assert!(path.exists(), "{name} command not written");
        let text = std::fs::read_to_string(&path).expect("read command");
        // Frontmatter opens the file — a banner above it would demote the
        // block to prose and the harness would advertise the banner as the
        // command's description. The banner follows it.
        assert!(text.starts_with("---\n"), "{name}: {text}");
        assert!(
            text.contains("<!-- Generated by `engram install`"),
            "{name}: unbannered"
        );
        assert!(text.contains("\ndescription: "), "{name}: no description");
        // The database must be pinned: without it the command would fall back
        // to clap's relative default and write to a different store.
        assert!(text.contains("--db /shared/engram.db"), "{name}: {text}");
        assert!(!text.contains("{{DB}}"), "{name} left a placeholder");
        assert!(!text.contains("{{HARNESS}}"), "{name} left a placeholder");
    }

    let before: Vec<_> = ["save-chat", "ingest", "context"]
        .iter()
        .map(|n| {
            std::fs::metadata(dir.join(format!("engram-{n}.md")))
                .expect("stat")
                .modified()
                .expect("mtime")
        })
        .collect();

    let assert = install(&db, &home, &["--db-path", "/shared/engram.db"])
        .assert()
        .success();
    let second = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(
        second["data"]["installed"], 0,
        "second run must write nothing"
    );
    for file in second["data"]["harnesses"][0]["files"]
        .as_array()
        .expect("files")
    {
        assert_eq!(file["outcome"], "unchanged");
    }

    let after: Vec<_> = ["save-chat", "ingest", "context"]
        .iter()
        .map(|n| {
            std::fs::metadata(dir.join(format!("engram-{n}.md")))
                .expect("stat")
                .modified()
                .expect("mtime")
        })
        .collect();
    assert_eq!(before, after, "an unchanged install must not touch mtimes");
}

#[test]
fn install_dry_run_creates_no_files() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create fake home");
    pretend_installed(&home, ".claude");

    let assert = install(&db, &home, &["--dry-run"]).assert().success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(json["metadata"]["dry_run"], true);
    assert_eq!(
        json["data"]["installed"], 3,
        "the plan still reports the work"
    );

    assert!(predicate::path::missing().eval(&home.join(".claude/commands")));
}

/// Engram writes into `$HOME`; it must not invent a home for software the
/// user has not installed.
#[test]
fn install_refuses_to_create_a_home_for_an_absent_harness() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create fake home (empty)");

    let assert = install(&db, &home, &[]).assert().success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(json["data"]["installed"], 0);

    for entry in json["data"]["harnesses"].as_array().expect("harnesses") {
        assert_eq!(entry["present"], false, "{entry}");
        assert!(entry["files"].as_array().expect("files").is_empty());
    }
    assert!(predicate::path::missing().eval(&home.join(".claude")));
    assert!(predicate::path::missing().eval(&home.join(".codex")));
}

/// A command file engram did not write belongs to the user.
#[test]
fn install_skips_files_it_did_not_write_unless_forced() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create fake home");
    pretend_installed(&home, ".claude");

    let dir = home.join(".claude/commands");
    std::fs::create_dir_all(&dir).expect("create commands dir");
    let hand_written = dir.join("engram-context.md");
    std::fs::write(&hand_written, "my own command, please do not clobber\n").expect("write");

    let assert = install(&db, &home, &[]).assert().success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(json["data"]["skipped"], 1);
    assert_eq!(
        std::fs::read_to_string(&hand_written).expect("still there"),
        "my own command, please do not clobber\n"
    );

    let skipped = json["data"]["harnesses"][0]["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|f| {
            f["path"]
                .as_str()
                .expect("path")
                .ends_with("engram-context.md")
        })
        .expect("the skipped file is reported");
    assert!(skipped["reason"]
        .as_str()
        .expect("reason")
        .contains("--force"));

    // With --force it is replaced.
    install(&db, &home, &["--force"]).assert().success();
    let text = std::fs::read_to_string(&hand_written).expect("read");
    assert!(text.starts_with("---\n"), "{text}");
    assert!(text.contains("<!-- Generated by `engram install`"));
}

/// Codex reads plain markdown prompts; frontmatter would render as literal
/// text at the top of every prompt.
#[test]
fn install_strips_frontmatter_for_harnesses_that_do_not_read_it() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create fake home");
    pretend_installed(&home, ".codex");
    pretend_installed(&home, ".claude");

    install(&db, &home, &["--db-path", "/shared/engram.db"])
        .assert()
        .success();

    let codex = std::fs::read_to_string(home.join(".codex/prompts/engram-save-chat.md"))
        .expect("codex prompt written");
    assert!(
        !codex.contains("argument-hint:"),
        "frontmatter leaked: {codex}"
    );
    assert!(codex.contains("--db /shared/engram.db"));
    assert!(codex.contains("--harness codex"));

    let claude = std::fs::read_to_string(home.join(".claude/commands/engram-save-chat.md"))
        .expect("claude command written");
    assert!(
        claude.contains("argument-hint:"),
        "claude code reads frontmatter"
    );
    assert!(claude.contains("--harness claude-code"));
}

/// The database is discovered from the harness's own MCP registration, which
/// is the store its agents actually share.
#[test]
fn install_discovers_the_database_from_the_harness_mcp_config() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create fake home");
    pretend_installed(&home, ".claude");
    std::fs::write(
        home.join(".claude.json"),
        r#"{"mcpServers":{"engram":{"type":"stdio","command":"engram",
            "args":["--db","/home/someone/.gemini/engram.db","mcp"]}}}"#,
    )
    .expect("write mcp config");

    let assert = install(&db, &home, &[]).assert().success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(
        json["data"]["harnesses"][0]["db"],
        "/home/someone/.gemini/engram.db"
    );

    let text = std::fs::read_to_string(home.join(".claude/commands/engram-ingest.md"))
        .expect("command written");
    assert!(
        text.contains("--db /home/someone/.gemini/engram.db"),
        "{text}"
    );
}

#[test]
fn install_reports_harnesses_with_no_command_surface() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).expect("create fake home");
    pretend_installed(&home, ".gemini/antigravity");

    let assert = install(&db, &home, &["--harness", "antigravity"])
        .assert()
        .success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    let entry = &json["data"]["harnesses"][0];
    assert_eq!(entry["harness"], "antigravity");
    assert!(entry["files"].as_array().expect("files").is_empty());
    assert_eq!(entry["note"], "no command surface engram can write");
}

#[test]
fn describe_advertises_install_and_ingest() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db).arg("describe").assert().success();
    let manifest = parse_json(&assert.get_output().stdout);
    let commands = manifest["commands"].as_array().expect("commands");
    assert!(commands.iter().any(|c| c == "ingest"));
    assert!(commands.iter().any(|c| c == "save-chat"));

    let ingest = &manifest["ingest"];
    assert_eq!(ingest["cli_only"], true);
    let excluded = ingest["excluded_by_default"].as_array().expect("array");
    assert!(excluded.iter().any(|e| e == "tool_result"));
    assert!(excluded.iter().any(|e| e == "thinking"));
}

// ------------------------------------------------------------- codex ingest

/// Plants the synthetic Codex rollout inside a fake `$HOME`.
///
/// Codex matches a session to a project by the `cwd` recorded in
/// `session_meta`, not by a mangled directory name, so the fixture's
/// `{{CWD}}` placeholder is substituted with the real temporary project path.
fn plant_codex_rollout(home: &Path, project: &Path) -> std::path::PathBuf {
    // The YYYY/MM/DD tree encodes the date, not the project.
    let dir = home.join(".codex/sessions/2026/06/25");
    std::fs::create_dir_all(&dir).expect("create fake codex sessions dir");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts/codex/rollout-basic.jsonl");
    let body = std::fs::read_to_string(&src)
        .expect("read fixture")
        .replace("{{CWD}}", &project.to_string_lossy());
    let dest = dir.join("rollout-2026-06-25T18-23-58-019eff61-7a56-7da1-b0ce-308ec7793715.jsonl");
    std::fs::write(&dest, body).expect("write rollout");
    dest
}

#[test]
fn codex_sessions_are_matched_by_recorded_cwd_not_by_a_mangled_name() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_codex_rollout(&home, &project);

    let assert = ingest(&db, &home, &project, &["--harness", "codex", "--list"])
        .assert()
        .success();
    let data = parse_single_line_json(&assert.get_output().stdout)["data"].clone();
    let sessions = data["sessions"].as_array().expect("sessions");
    assert_eq!(sessions.len(), 1);
    // The id is per-rollout (file name), not per-conversation: Codex reuses
    // `session_meta.session_id` across resumed sessions.
    assert_eq!(
        sessions[0]["session_id"],
        "2026-06-25T18-23-58-019eff61-7a56-7da1-b0ce-308ec7793715"
    );

    // A different project must not match the same rollout.
    let other = tmp.path().join("other-project");
    std::fs::create_dir_all(&other).expect("create other project");
    let assert = ingest(&db, &home, &other, &["--harness", "codex", "--list"])
        .assert()
        .success();
    let sessions = parse_single_line_json(&assert.get_output().stdout)["data"]["sessions"]
        .as_array()
        .expect("sessions")
        .len();
    assert_eq!(sessions, 0, "cwd matching must not be fuzzy");
}

#[test]
fn codex_ingest_prefers_the_display_channel_and_is_idempotent() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_codex_rollout(&home, &project);

    let args = ["--harness", "codex", "--scope", "cx"];
    let assert = ingest(&db, &home, &project, &args).assert().success();
    let data = parse_single_line_json(&assert.get_output().stdout)["data"].clone();

    // Four turns: two user messages, two agent messages. The response_item
    // duplicates and the injected <environment_context> must not appear.
    assert_eq!(data["inserted"], 4, "unexpected turn count: {data}");
    assert_eq!(data["filtered"]["thinking"], 1);
    assert_eq!(data["filtered"]["tool_use"], 1);
    assert_eq!(
        data["filtered"]["tool_result"], 2,
        "function output + patch_apply_end"
    );
    assert_eq!(
        data["filtered"]["unknown_record"], 1,
        "the unknown event is counted"
    );

    let rows = recall_data(&db, "cx");
    let all: String = rows
        .iter()
        .map(|r| r["content"].as_str().expect("content").to_string())
        .collect::<Vec<_>>()
        .join("\n");

    // The reason event_msg is the primary channel: the raw channel carries an
    // injected block that nobody said.
    assert!(
        !all.contains("<environment_context>"),
        "injected block stored: {all}"
    );
    assert!(
        !all.contains("developer instructions"),
        "developer message stored: {all}"
    );
    // Each real message appears exactly once despite being in both channels.
    assert_eq!(
        all.matches("Decided: stream the rollout").count(),
        1,
        "message duplicated across channels: {all}"
    );
    // Both agent_message phases are kept.
    assert!(
        all.contains("Checking the rollout sizes first."),
        "commentary dropped"
    );
    assert!(
        all.contains("Decided: stream the rollout"),
        "final_answer dropped"
    );
    // Thinking, tool payloads, and credentials never land.
    assert!(
        !all.contains("internal deliberation"),
        "thinking stored: {all}"
    );
    assert!(!all.contains("hunter2"), "tool payload stored: {all}");
    assert!(!all.contains("ghp_AAAA"), "credential stored: {all}");
    assert!(all.contains("[redacted:github-token]"));

    assert!(rows.iter().all(|r| r["agent"] == "codex"));
    let roles: Vec<&str> = rows
        .iter()
        .map(|r| r["role"].as_str().expect("role"))
        .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);

    // Re-ingesting inserts nothing: ids are content-and-position derived.
    let assert = ingest(&db, &home, &project, &args).assert().success();
    let second = parse_single_line_json(&assert.get_output().stdout)["data"].clone();
    assert_eq!(second["inserted"], 0);
    assert_eq!(second["skipped_existing"], 4);
}

/// Rollouts of 114 MB exist in the wild; the ceiling must refuse rather than
/// read one by surprise, and say how to override.
#[test]
fn codex_refuses_a_transcript_above_the_byte_ceiling() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_codex_rollout(&home, &project);

    let assert = ingest(
        &db,
        &home,
        &project,
        &["--harness", "codex", "--scope", "cx", "--max-bytes", "10"],
    )
    .assert()
    .failure()
    .code(2);

    assert!(assert.get_output().stdout.is_empty());
    let err = parse_single_line_json(&assert.get_output().stderr);
    assert_eq!(err["error"]["code"], "INVALID_ARGUMENT");
    assert!(err["error"]["message"]
        .as_str()
        .expect("message")
        .contains("ceiling"));
    assert!(err["error"]["hint"]
        .as_str()
        .expect("hint")
        .contains("--max-bytes"));
}

/// Both readers must land in one scope without either taking over.
#[test]
fn codex_and_claude_code_transcripts_coexist_in_one_scope() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);
    plant_claude_transcript(&home, &project, "session-basic");
    plant_codex_rollout(&home, &project);

    // With two readable harnesses installed, engram must refuse to guess.
    let assert = ingest(&db, &home, &project, &["--scope", "both"])
        .assert()
        .failure()
        .code(2);
    let err = parse_single_line_json(&assert.get_output().stderr);
    assert!(err["error"]["message"]
        .as_str()
        .expect("message")
        .contains("will not guess"));

    ingest(
        &db,
        &home,
        &project,
        &["--harness", "claude-code", "--scope", "both"],
    )
    .assert()
    .success();
    ingest(
        &db,
        &home,
        &project,
        &["--harness", "codex", "--scope", "both"],
    )
    .assert()
    .success();

    let rows = recall_data(&db, "both");
    assert_eq!(rows.len(), 8, "four turns from each harness");
    let agents: std::collections::BTreeSet<&str> = rows
        .iter()
        .map(|r| r["agent"].as_str().expect("agent"))
        .collect();
    assert_eq!(
        agents,
        ["claude-code", "codex"].into_iter().collect(),
        "each turn is attributed to the harness it came from"
    );
}

/// Codex reuses `session_meta.session_id` when a conversation is resumed:
/// three rollout files sharing one id were observed on a real machine. If
/// engram keyed turns on that id, a turn at the same line index in the second
/// rollout would derive the same id as one in the first, and `INSERT OR
/// IGNORE` would drop it without a word.
#[test]
fn codex_resumed_sessions_do_not_swallow_each_others_turns() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let (home, project) = ingest_fixture(&tmp);

    // Two rollouts, same session_id inside, different file names — exactly
    // what resuming produces.
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts/codex/rollout-basic.jsonl");
    let body = std::fs::read_to_string(&src)
        .expect("read fixture")
        .replace("{{CWD}}", &project.to_string_lossy());
    let dir = home.join(".codex/sessions/2026/06/25");
    std::fs::create_dir_all(&dir).expect("create sessions dir");
    for stamp in ["2026-06-25T18-23-58", "2026-06-25T19-40-02"] {
        let name = format!("rollout-{stamp}-019eff61-7a56-7da1-b0ce-308ec7793715.jsonl");
        std::fs::write(dir.join(name), &body).expect("write rollout");
    }

    let assert = ingest(
        &db,
        &home,
        &project,
        &["--harness", "codex", "--session", "all", "--scope", "cx"],
    )
    .assert()
    .success();
    let data = parse_single_line_json(&assert.get_output().stdout)["data"].clone();

    assert_eq!(
        data["sessions"].as_array().expect("sessions").len(),
        2,
        "both rollouts must be listed separately"
    );
    assert_eq!(
        data["inserted"], 8,
        "each rollout contributes its own four turns: {data}"
    );
    assert_eq!(data["skipped_existing"], 0, "no turn may be swallowed");
    assert_eq!(recall_data(&db, "cx").len(), 8);
}

// -------------------------------------------------------------------- hooks

#[test]
fn install_hooks_merges_without_disturbing_the_rest_of_settings() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("create fake home");

    // A settings file the user hand-wrote, with a deliberately non-alphabetical
    // key order and a SessionEnd hook that is not engram's.
    let original = r#"{
  "theme": "auto",
  "model": "opus",
  "permissions": {
    "defaultMode": "auto"
  },
  "hooks": {
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "notify-send 'session over'"
          }
        ]
      }
    ]
  },
  "aardvark": true
}
"#;
    let settings = home.join(".claude/settings.json");
    std::fs::write(&settings, original).expect("write settings");

    let assert = install(&db, &home, &["--hooks", "--db-path", "/shared/engram.db"])
        .assert()
        .success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    let hook = &json["data"]["hooks"][0];
    assert_eq!(hook["harness"], "claude-code");
    assert_eq!(hook["outcome"], "updated");
    assert!(hook["backup"]
        .as_str()
        .is_some_and(|b| b.contains("engram-backup")));

    let updated: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).expect("read")).expect("json");

    // Sibling keys survive, values and all.
    assert_eq!(updated["theme"], "auto");
    assert_eq!(updated["model"], "opus");
    assert_eq!(updated["permissions"]["defaultMode"], "auto");
    assert_eq!(updated["aardvark"], true);

    // Key order survives — the whole reason serde_json is built with
    // preserve_order. Alphabetizing someone's config as a side effect of
    // adding one hook would be engram reformatting a file it does not own.
    let keys: Vec<&str> = updated
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["theme", "model", "permissions", "hooks", "aardvark"]);

    // The user's own SessionEnd hook is untouched, and engram's was appended.
    let entries = updated["hooks"]["SessionEnd"].as_array().expect("array");
    assert_eq!(
        entries.len(),
        2,
        "the foreign hook must survive: {entries:?}"
    );
    assert_eq!(
        entries[0]["hooks"][0]["command"],
        "notify-send 'session over'"
    );
    let ours = entries[1]["hooks"][0]["command"].as_str().expect("command");
    assert!(ours.contains("--db /shared/engram.db"), "{ours}");
    // Capture only. Writing an archive into someone's repo unasked is exactly
    // the automatic behavior that was ruled out.
    assert!(ours.contains("ingest"), "{ours}");
    assert!(
        !ours.contains("save-chat"),
        "a hook must never archive: {ours}"
    );

    // The backup holds the original, byte for byte.
    let backup = hook["backup"].as_str().expect("backup path");
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        original
    );
}

#[test]
fn install_hooks_is_idempotent_and_creates_missing_settings() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("create fake home");
    let settings = home.join(".claude/settings.json");

    let assert = install(&db, &home, &["--hooks"]).assert().success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(json["data"]["hooks"][0]["outcome"], "created");
    assert!(settings.exists());

    let after_first = std::fs::read_to_string(&settings).expect("read");

    let assert = install(&db, &home, &["--hooks"]).assert().success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    assert_eq!(
        json["data"]["hooks"][0]["outcome"], "unchanged",
        "re-running must not re-append the hook"
    );
    assert_eq!(
        std::fs::read_to_string(&settings).expect("read"),
        after_first
    );

    let entries: Value = serde_json::from_str(&after_first).expect("json");
    assert_eq!(
        entries["hooks"]["SessionEnd"]
            .as_array()
            .expect("array")
            .len(),
        1,
        "exactly one engram hook, however many times install runs"
    );
}

#[test]
fn install_hooks_dry_run_shows_the_fragment_and_writes_nothing() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("create fake home");

    let assert = install(&db, &home, &["--hooks", "--dry-run"])
        .assert()
        .success();
    let hook = parse_single_line_json(&assert.get_output().stdout)["data"]["hooks"][0].clone();
    assert_eq!(hook["outcome"], "created");
    assert_eq!(hook["dry_run"], true);

    // The exact fragment is shown, not described — this is a settings file
    // engram does not own.
    let command = hook["fragment"]["hooks"][0]["command"]
        .as_str()
        .expect("fragment command");
    assert!(command.starts_with("engram "), "{command}");
    assert!(command.contains("ingest"));
    assert_eq!(hook["fragment"]["hooks"][0]["timeout"], 30);

    assert!(predicate::path::missing().eval(&home.join(".claude/settings.json")));
}

/// Engram must not overwrite a settings file it could not parse — that would
/// destroy a config while trying to add one line to it.
#[test]
fn install_hooks_refuses_a_malformed_settings_file() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("create fake home");
    let settings = home.join(".claude/settings.json");
    let broken = "{ this is not json at all\n";
    std::fs::write(&settings, broken).expect("write");

    let assert = install(&db, &home, &["--hooks"]).assert().failure().code(1);
    let err = parse_single_line_json(&assert.get_output().stderr);
    assert!(err["error"]["message"]
        .as_str()
        .expect("message")
        .contains("not valid JSON"));

    assert_eq!(
        std::fs::read_to_string(&settings).expect("read"),
        broken,
        "a file engram cannot parse must be left exactly as it was"
    );
}

/// Only Claude Code has a hook system engram can write; the rest say so.
#[test]
fn install_hooks_reports_harnesses_with_no_hook_system() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".codex")).expect("create fake home");

    let assert = install(&db, &home, &["--harness", "codex", "--hooks"])
        .assert()
        .success();
    let hook = &parse_single_line_json(&assert.get_output().stdout)["data"]["hooks"][0];
    assert_eq!(hook["harness"], "codex");
    assert_eq!(
        hook["note"],
        "this harness has no hook system engram can write"
    );
    assert!(predicate::path::missing().eval(&home.join(".codex/settings.json")));
}

/// Without `--hooks`, no settings file is touched or even considered.
#[test]
fn install_without_hooks_never_touches_settings() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("create fake home");

    let assert = install(&db, &home, &[]).assert().success();
    let json = parse_single_line_json(&assert.get_output().stdout);
    assert!(
        json["data"].get("hooks").is_none(),
        "the hooks key must be absent, not empty: {}",
        json["data"]
    );
    assert!(predicate::path::missing().eval(&home.join(".claude/settings.json")));
}

/// The MCP ledger is stated in four places — the manual, `CLAUDE.md`, the
/// module doc, and `describe`. This pins the machine-readable one so a
/// forgotten eleventh tool fails a test rather than quietly doubling every
/// conversation's schema cost.
#[test]
fn describe_pins_the_mcp_tool_ceiling() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");

    let assert = engram(&db).arg("describe").assert().success();
    let manifest = parse_json(&assert.get_output().stdout);
    let mcp = &manifest["mcp"];

    let tools = mcp["tools"].as_array().expect("tools array");
    assert_eq!(
        tools.len(),
        10,
        "the tool surface is capped at ten; an eleventh must displace an existing one \
         and be argued in doc/engram.texi"
    );
    assert_eq!(mcp["ceiling"], 10);
    assert_eq!(mcp["ceiling_reached"], true);
    assert!(tools.iter().any(|t| t == "save_chat"));

    // Commands that must stay off the agent-invocable surface.
    let cli_only = mcp["cli_only"].as_array().expect("cli_only array");
    for command in ["install", "ingest", "consolidate", "index", "rule purge"] {
        assert!(
            cli_only.iter().any(|c| c == command),
            "{command} must remain CLI-only"
        );
    }
}

/// Every `!`-prefixed shell line in an *installed* command file must actually
/// run — with no arguments supplied, which is how a slash command is usually
/// invoked.
///
/// This is the test that was missing. `install` was verified to write correct
/// *files*, and the CLI was verified to accept correct *flags*, but nothing
/// executed what the files contained. The gap let `save-chat --scope $1` ship:
/// with no argument the flag arrived without a value and clap rejected it
/// before engram's scope cascade was ever reached.
#[test]
fn installed_command_shell_lines_run_with_no_arguments() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("test.db");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("create fake home");

    // A scope must exist for save-chat to have anything to archive. The scope
    // name must match what the cascade resolves to inside `tmp`, since the
    // whole point is that the command line supplies no scope of its own.
    let scope = tmp
        .path()
        .file_name()
        .expect("tempdir basename")
        .to_string_lossy()
        .into_owned();
    remember(&db, "t", &scope, "a decision");
    install(&db, &home, &[]).assert().success();

    let exe = env!("CARGO_BIN_EXE_engram");
    let dir = home.join(".claude/commands");
    for name in ["save-chat", "ingest", "context"] {
        let text =
            std::fs::read_to_string(dir.join(format!("engram-{name}.md"))).expect("read command");
        for line in text.lines() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("!`") else {
                continue;
            };
            let Some(cmd) = rest.strip_suffix('`') else {
                continue;
            };
            // How a harness expands a command invoked bare.
            let expanded = cmd.replace("$1", "").replace("$ARGUMENTS", "");
            // Never mutate the real filesystem or a live session from a test.
            if expanded.contains("ingest") && !expanded.contains("--list") {
                continue;
            }
            let argv = shell_words(&expanded);
            assert_eq!(argv.first().map(String::as_str), Some("engram"), "{cmd}");

            let out = std::process::Command::new(exe)
                .args(&argv[1..])
                .current_dir(tmp.path())
                .env("HOME", &home)
                .output()
                .expect("run command line");
            let stderr = String::from_utf8_lossy(&out.stderr);
            // `ingest --list` in a bare tempdir legitimately finds no session;
            // what must never happen is an argument-parsing failure.
            assert!(
                !stderr.contains("a value is required")
                    && !stderr.contains("unexpected argument")
                    && !stderr.contains("required arguments were not provided"),
                "{name}: installed command line fails to parse: `{cmd}`\n{stderr}"
            );
        }
    }
}

/// Splits a command line on whitespace, honouring double quotes. Enough for
/// the shapes engram's own templates use.
fn shell_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for ch in line.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}
