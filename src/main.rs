// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Engram — shared verbatim chat memory for multi-model LLM pipelines.
//! Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
//! https://Engram.SpacecraftSoftware.org/

mod cli;
mod error;
mod http;
mod mcp;
mod output;
mod rules;
mod store;
mod time;

use clap::Parser;
use cli::{Cli, Command, RuleAction};
use error::AppError;
use output::envelope::Response;
use output::mode::{resolve_mode, OutputMode};
use std::sync::{Arc, Mutex};
use store::Store;

fn main() {
    let cli = Cli::parse();
    let mode = resolve_mode(cli.format, cli.json, cli.no_color, cli.accessible);

    let store = match Store::open(&cli.db) {
        Ok(s) => Arc::new(Mutex::new(s)),
        Err(e) => {
            AppError::from(e).emit_to_stderr();
            std::process::exit(1);
        }
    };

    let exit_code = run(cli.command, store, mode);
    std::process::exit(exit_code);
}

fn run(command: Command, store: Arc<Mutex<Store>>, mode: OutputMode) -> i32 {
    match command {
        Command::Remember {
            agent,
            scope,
            role,
            dry_run,
            content,
        } => {
            let content = match content.or_else(read_stdin) {
                Some(c) => c,
                None => {
                    return fail(
                        AppError::new(
                            error::ErrorCode::InvalidArgument,
                            2,
                            "no content provided",
                            "pass content as an argument or pipe it via stdin",
                        ),
                        mode,
                    )
                }
            };
            if dry_run {
                // Validation passed; echo the would-be memory without writing.
                let plan = serde_json::json!({
                    "actions": [{
                        "action": "remember",
                        "agent": agent,
                        "scope": scope,
                        "role": role,
                        "content": content,
                    }],
                    "summary": format!(
                        "Would store 1 '{role}' memory ({} chars) in scope '{scope}'",
                        content.chars().count()
                    ),
                });
                emit_ok(
                    Response::new("engram memory remember --dry-run", plan).with_dry_run(),
                    mode,
                );
                return 0;
            }
            let guard = store.lock().expect("store lock poisoned");
            match guard.remember(&agent, &scope, &role, &content) {
                Ok(mem) => {
                    emit_ok(Response::new("engram memory remember", mem), mode);
                    0
                }
                Err(e) => {
                    let err = AppError::from(e);
                    emit_error(&err, mode);
                    err.exit_code
                }
            }
        }
        Command::Recall { scope, limit } => {
            let guard = store.lock().expect("store lock poisoned");
            match guard.recall(&scope, limit) {
                Ok(mems) => {
                    emit_ok(Response::new("engram memory recall", mems), mode);
                    0
                }
                Err(e) => {
                    let err = AppError::from(e);
                    emit_error(&err, mode);
                    err.exit_code
                }
            }
        }
        Command::Search {
            query,
            scope,
            limit,
        } => {
            let guard = store.lock().expect("store lock poisoned");
            match guard.search(&query, scope.as_deref(), limit) {
                Ok(mems) => {
                    emit_ok(Response::new("engram memory search", mems), mode);
                    0
                }
                Err(e) => {
                    let err = AppError::from(e);
                    emit_error(&err, mode);
                    err.exit_code
                }
            }
        }
        Command::Rule { action } => run_rule(action, &store, mode),
        Command::Mcp => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(e) = rt.block_on(mcp::run_stdio(store)) {
                eprintln!("{{\"error\":{{\"message\":\"{e}\"}}}}");
                return 1;
            }
            0
        }
        Command::Serve { addr } => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async {
                let app = http::router(store);
                let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind address");
                eprintln!("engram: serving on http://{addr} (local-only; no auth — do not expose beyond localhost)");
                axum::serve(listener, app).await.expect("serve");
            });
            0
        }
        Command::Schema => {
            let schema = serde_json::json!({
                "Memory": schemars::schema_for!(store::Memory),
                "Rule": schemars::schema_for!(store::Rule),
            });
            println!("{}", serde_json::to_string_pretty(&schema).unwrap());
            0
        }
        Command::Describe => {
            let manifest = serde_json::json!({
                "tool": "engram",
                "version": env!("CARGO_PKG_VERSION"),
                "maintainer": "Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>",
                "website": "https://Engram.SpacecraftSoftware.org/",
                "commands": [
                    "remember", "recall", "search",
                    "rule add", "rule list", "rule retire", "rule sync",
                    "save-chat", "mcp", "serve", "schema", "describe"
                ],
                "transports": ["cli", "mcp-stdio", "http"],
                "storage": "sqlite+fts5, single shared file",
                "output": {
                    "formats": ["json", "jsonl", "csv"],
                    "jsonl": "first line is the metadata envelope with data:null, then one line per record",
                    "csv": "RFC 4180 data rows on stdout; metadata as one JSON line on stderr",
                    "dry_run": "remember and rule sync accept --dry-run; the envelope carries metadata.dry_run=true",
                    "accessible": "--accessible or SPACECRAFT_A11Y=1 (Standard §18); status tags [OK]/[ERROR] are present in every human mode"
                },
                "rules": {
                    "description": "Durable project policy, stored once and rendered into the \
                                    markdown files agent harnesses auto-load.",
                    "scope_resolution": ["--scope", "ENGRAM_SCOPE", "git-root-basename", "cwd-basename"],
                    "sync_targets": rules::DEFAULT_TARGETS,
                    "surfaces": ["cli", "mcp-stdio", "http"],
                    "retire": "tombstones rather than deletes; re-adding the same id reinstates",
                    "http_routes": [
                        "POST /v1/rules", "GET /v1/rules", "DELETE /v1/rules/:rule_id",
                        "POST /v1/rules/sync"
                    ],
                    "mcp_tools": ["rule_add", "rule_list", "rule_retire", "rule_sync"]
                }
            });
            println!("{}", serde_json::to_string_pretty(&manifest).unwrap());
            0
        }
        Command::SaveChat { scope, file, model } => {
            let guard = store.lock().expect("store lock poisoned");
            match guard.recall(&scope, u32::MAX) {
                Ok(mems) => {
                    if mems.is_empty() {
                        let err = AppError::new(
                            error::ErrorCode::NotFound,
                            3,
                            format!("no memories found for scope '{}'", scope),
                            "verify the scope name is correct",
                        );
                        emit_error(&err, mode);
                        return err.exit_code;
                    }

                    match handle_save_chat(&scope, mems, file, model) {
                        Ok(result) => {
                            emit_ok(Response::new("engram memory save-chat", result), mode);
                            0
                        }
                        Err(e) => {
                            let err = AppError::new(
                                error::ErrorCode::InternalError,
                                1,
                                format!("failed to save chat: {}", e),
                                "check filesystem permissions and directory configuration",
                            );
                            emit_error(&err, mode);
                            err.exit_code
                        }
                    }
                }
                Err(e) => {
                    let err = AppError::from(e);
                    emit_error(&err, mode);
                    err.exit_code
                }
            }
        }
    }
}

#[derive(serde::Serialize)]
struct RuleAddResult {
    rule: store::Rule,
    /// False when an existing rule with this id was revised in place.
    created: bool,
    scope_origin: rules::ScopeOrigin,
    /// Storing a rule does not put it in front of any model. Say so, every
    /// time — an agent reading this envelope should know the job is half done.
    next_step: &'static str,
}

#[derive(serde::Serialize)]
struct RuleListResult {
    scope: String,
    scope_origin: rules::ScopeOrigin,
    count: usize,
    rules: Vec<store::Rule>,
}

#[derive(serde::Serialize)]
struct RuleRetireResult {
    #[serde(flatten)]
    retire: store::RuleRetire,
    scope_origin: rules::ScopeOrigin,
    /// Retiring changes the database, not the markdown. Say so.
    next_step: &'static str,
}

#[derive(serde::Serialize)]
struct RuleSyncResult {
    scope: String,
    scope_origin: rules::ScopeOrigin,
    root: String,
    rule_count: usize,
    dry_run: bool,
    files: Vec<rules::SyncedFile>,
}

fn run_rule(action: RuleAction, store: &Arc<Mutex<Store>>, mode: OutputMode) -> i32 {
    match action {
        RuleAction::Add {
            id,
            scope,
            agent,
            text,
        } => {
            let text = match text {
                Some(t) => t,
                None => match read_stdin() {
                    Some(t) => t,
                    None => {
                        return fail(
                            AppError::new(
                                error::ErrorCode::InvalidArgument,
                                2,
                                "no rule text provided",
                                "pass the rule as an argument or pipe it via stdin",
                            ),
                            mode,
                        )
                    }
                },
            };
            if let Err(reason) = rules::validate_rule_id(&id) {
                return fail(
                    AppError::new(
                        error::ErrorCode::InvalidArgument,
                        2,
                        reason,
                        "use a short kebab-case id, e.g. skill-description-1000",
                    ),
                    mode,
                );
            }
            if let Err(reason) = rules::validate_rule_text(&text) {
                return fail(
                    AppError::new(
                        error::ErrorCode::InvalidArgument,
                        2,
                        reason,
                        "state the rule as plain prose",
                    ),
                    mode,
                );
            }

            let resolved = match rules::resolve_scope(scope.as_deref()) {
                Ok(r) => r,
                Err(e) => return fail(scope_error(e), mode),
            };
            let guard = store.lock().expect("store lock poisoned");
            match guard.rule_add(&agent, &resolved.name, &id, text.trim()) {
                Ok(upsert) => {
                    let result = RuleAddResult {
                        rule: upsert.rule,
                        created: upsert.created,
                        scope_origin: resolved.origin,
                        next_step: "run `engram rule sync` to render this rule into AGENTS.md and CLAUDE.md; \
                                    until then no agent will read it",
                    };
                    emit_ok(Response::new("engram rule add", result), mode);
                    0
                }
                Err(e) => fail(AppError::from(e), mode),
            }
        }

        RuleAction::List {
            scope,
            include_retired,
        } => {
            let resolved = match rules::resolve_scope(scope.as_deref()) {
                Ok(r) => r,
                Err(e) => return fail(scope_error(e), mode),
            };
            let guard = store.lock().expect("store lock poisoned");
            let listed = if include_retired {
                guard.rules_including_retired(&resolved.name)
            } else {
                guard.rules(&resolved.name)
            };
            match listed {
                Ok(rules) => {
                    let result = RuleListResult {
                        scope: resolved.name,
                        scope_origin: resolved.origin,
                        count: rules.len(),
                        rules,
                    };
                    emit_ok(Response::new("engram rule list", result), mode);
                    0
                }
                Err(e) => fail(AppError::from(e), mode),
            }
        }

        RuleAction::Retire { id, scope } => {
            let resolved = match rules::resolve_scope(scope.as_deref()) {
                Ok(r) => r,
                Err(e) => return fail(scope_error(e), mode),
            };
            let guard = store.lock().expect("store lock poisoned");
            match guard.rule_retire(&resolved.name, &id) {
                Ok(retire) => {
                    // A missing id is a real error: the caller believes it is
                    // withdrawing something and it is not.
                    if retire.outcome == store::RetireOutcome::NotFound {
                        return fail(
                            AppError::new(
                                error::ErrorCode::NotFound,
                                3,
                                format!("no rule '{id}' in scope '{}'", resolved.name),
                                "list the ids with `engram rule list --include-retired`",
                            ),
                            mode,
                        );
                    }
                    let payload = RuleRetireResult {
                        retire,
                        scope_origin: resolved.origin,
                        next_step: "run `engram rule sync` to drop this rule from AGENTS.md and \
                                    CLAUDE.md; until then the synced files still assert it",
                    };
                    emit_ok(Response::new("engram rule retire", payload), mode);
                    0
                }
                Err(e) => fail(AppError::from(e), mode),
            }
        }

        RuleAction::Sync {
            scope,
            files,
            dry_run,
        } => {
            let resolved = match rules::resolve_scope(scope.as_deref()) {
                Ok(r) => r,
                Err(e) => return fail(scope_error(e), mode),
            };
            let rule_list = {
                let guard = store.lock().expect("store lock poisoned");
                match guard.rules(&resolved.name) {
                    Ok(r) => r,
                    Err(e) => return fail(AppError::from(e), mode),
                }
            };

            let block = rules::render_block(&resolved.name, &rule_list);
            let mut written = Vec::new();
            for path in rules::target_paths(&resolved.root, &files) {
                match rules::sync_file(&path, &block, dry_run) {
                    Ok(file) => written.push(file),
                    Err(e) => {
                        return fail(
                            AppError::new(
                                error::ErrorCode::InternalError,
                                1,
                                format!("failed to sync {}: {e}", path.display()),
                                "check filesystem permissions for the project root",
                            ),
                            mode,
                        )
                    }
                }
            }

            let result = RuleSyncResult {
                scope: resolved.name,
                scope_origin: resolved.origin,
                root: resolved.root.to_string_lossy().into_owned(),
                rule_count: rule_list.len(),
                dry_run,
                files: written,
            };
            let resp = Response::new("engram rule sync", result);
            // metadata.dry_run mirrors data.dry_run so the envelope contract
            // is uniform with `remember --dry-run` (validation-safety §4).
            let resp = if dry_run { resp.with_dry_run() } else { resp };
            emit_ok(resp, mode);
            0
        }
    }
}

/// Reads all of stdin, returning `None` when it is empty or unreadable.
fn read_stdin() -> Option<String> {
    use std::io::Read;
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() || buf.trim().is_empty() {
        return None;
    }
    Some(buf)
}

fn scope_error(e: std::io::Error) -> AppError {
    AppError::new(
        error::ErrorCode::InternalError,
        1,
        format!("could not resolve scope: {e}"),
        "pass --scope explicitly, or set ENGRAM_SCOPE",
    )
}

/// Emits an error in the active mode and yields its exit code.
fn fail(err: AppError, mode: OutputMode) -> i32 {
    emit_error(&err, mode);
    err.exit_code
}

#[derive(serde::Serialize)]
struct SaveChatResult {
    scope: String,
    file_path: String,
    signed_by: String,
    messages_saved: usize,
}

fn handle_save_chat(
    scope: &str,
    mems: Vec<store::Memory>,
    custom_file: Option<std::path::PathBuf>,
    model: Option<String>,
) -> Result<SaveChatResult, Box<dyn std::error::Error>> {
    let current_dir = std::env::current_dir()?;

    // Ensure chat directory exists
    let chat_dir = current_dir.join("chat");
    std::fs::create_dir_all(&chat_dir)?;

    // Ensure chat/ is gitignored
    let gitignore_path = current_dir.join(".gitignore");
    let mut gitignore_content = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };

    if !gitignore_content
        .lines()
        .any(|l| l.trim() == "chat" || l.trim() == "chat/")
    {
        if !gitignore_content.ends_with('\n') && !gitignore_content.is_empty() {
            gitignore_content.push('\n');
        }
        gitignore_content.push_str("chat/\n");
        std::fs::write(&gitignore_path, &gitignore_content)?;
    }

    // Resolve model name
    let model_name = model
        .or_else(|| std::env::var("MODEL").ok())
        .or_else(|| std::env::var("LLM_MODEL").ok())
        .or_else(|| std::env::var("AI_AGENT").ok())
        .or_else(|| std::env::var("AGENT").ok())
        .unwrap_or_else(|| "gemini-3.5-flash-high".to_string());

    // Resolve target file
    let target_file = match custom_file {
        Some(f) => f,
        None => {
            let timestamp = crate::time::now_iso8601().replace(':', "-");
            chat_dir.join(format!("{}.texi", timestamp))
        }
    };

    let exists = target_file.exists();
    let mut file_content = String::new();

    if exists {
        // Read existing content and strip trailing @bye to support clean append
        let existing = std::fs::read_to_string(&target_file)?;
        let trimmed = existing.trim();
        if let Some(stripped) = trimmed.strip_suffix("@bye") {
            file_content = stripped.to_string();
        } else {
            file_content = existing;
        }
    } else {
        // Write header
        file_content.push_str("\\input texinfo\n");
        file_content.push_str(&format!("@settitle Chat history for scope: {}\n\n", scope));
    }

    // Sign the block
    let sign_timestamp = crate::time::now_iso8601();
    file_content.push_str(&format!(
        "@c Signed by: {} on {}\n",
        model_name, sign_timestamp
    ));
    file_content.push_str(&format!("@chapter Chat history for scope: {}\n\n", scope));

    let num_saved = mems.len();
    for mem in mems {
        file_content.push_str(&format!(
            "@section Message by {} ({}) at {}\n",
            mem.agent, mem.role, mem.created_at
        ));
        file_content.push_str(&format!("{}\n\n", escape_texinfo(&mem.content)));
    }

    file_content.push_str("@bye\n");
    std::fs::write(&target_file, file_content)?;

    Ok(SaveChatResult {
        scope: scope.to_string(),
        file_path: target_file.to_string_lossy().to_string(),
        signed_by: model_name,
        messages_saved: num_saved,
    })
}

fn escape_texinfo(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '@' => out.push_str("@@"),
            '{' => out.push_str("@{"),
            '}' => out.push_str("@}"),
            _ => out.push(c),
        }
    }
    out
}

// Rust guideline compliant 2026-05-18

fn emit_ok<T: serde::Serialize>(resp: Response<T>, mode: OutputMode) {
    match mode {
        OutputMode::Json => {
            println!(
                "{}",
                serde_json::to_string(&resp).expect("Response serializes")
            );
        }
        OutputMode::Jsonl => emit_jsonl(&resp),
        OutputMode::Csv => emit_csv(&resp),
        OutputMode::HumanWithColor | OutputMode::HumanNoColor => {
            // Status tag to stderr (never color-only, §18.2.1); the data
            // payload alone goes to stdout (§7 stdout/stderr separation).
            let command = resp.metadata.command.clone();
            if mode == OutputMode::HumanWithColor {
                use owo_colors::OwoColorize;
                // Acid Lime per the Steelbore Modern palette (§11).
                eprintln!("{} {command}", "[OK]".truecolor(180, 255, 0).bold());
            } else {
                eprintln!("[OK] {command}");
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&resp).expect("Response serializes")
            );
        }
    }
}

/// `--format jsonl`: first line is the envelope metadata with `data: null`,
/// then one line per record (list payloads) or one line with the whole
/// payload (object payloads). Documented in `engram describe`.
fn emit_jsonl<T: serde::Serialize>(resp: &Response<T>) {
    let value = serde_json::to_value(resp).expect("Response serializes");
    let metadata = &value["metadata"];
    println!(
        "{}",
        serde_json::json!({ "metadata": metadata, "data": null })
    );
    match &value["data"] {
        serde_json::Value::Array(items) => {
            for item in items {
                println!("{item}");
            }
        }
        data => println!("{data}"),
    }
}

/// `--format csv`: RFC 4180 rows of the data records on stdout; the envelope
/// metadata goes to stderr as one line of JSON (CSV has no envelope).
fn emit_csv<T: serde::Serialize>(resp: &Response<T>) {
    let value = serde_json::to_value(resp).expect("Response serializes");
    eprintln!("{}", serde_json::json!({ "metadata": &value["metadata"] }));
    let records: Vec<serde_json::Map<String, serde_json::Value>> = match &value["data"] {
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|i| i.as_object().cloned())
            .collect(),
        serde_json::Value::Object(obj) => vec![obj.clone()],
        other => {
            println!("value");
            println!("{}", csv_escape(&scalar_to_string(other)));
            return;
        }
    };
    let Some(first) = records.first() else { return };
    // serde_json preserves struct field order, so the first record's keys are
    // the stable header. Records missing a key emit an empty field.
    let headers: Vec<&String> = first.keys().collect();
    println!(
        "{}",
        headers
            .iter()
            .map(|h| csv_escape(h))
            .collect::<Vec<_>>()
            .join(",")
    );
    for record in &records {
        let row: Vec<String> = headers
            .iter()
            .map(|h| {
                record
                    .get(*h)
                    .map(|v| csv_escape(&scalar_to_string(v)))
                    .unwrap_or_default()
            })
            .collect();
        println!("{}", row.join(","));
    }
}

fn scalar_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// RFC 4180: quote fields containing commas, quotes, or line breaks; double
/// embedded quotes.
fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

fn emit_error(err: &AppError, mode: OutputMode) {
    if mode.is_machine() {
        err.emit_to_stderr();
    } else {
        err.emit_human(mode == OutputMode::HumanWithColor);
    }
}
