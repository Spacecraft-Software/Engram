// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Engram — shared verbatim chat memory for multi-model LLM pipelines.
//! Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
//! https://Engram.SpacecraftSoftware.org/

mod cli;
mod embed;
mod error;
mod facts;
mod http;
mod mcp;
mod output;
mod retrieval;
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

    // `--model-path` only exists in vector builds; the default build passes
    // None so `run` keeps one signature across feature sets.
    #[cfg(feature = "vector")]
    let model_path = cli.model_path.clone();
    #[cfg(not(feature = "vector"))]
    let model_path: Option<std::path::PathBuf> = None;

    let exit_code = run(cli.command, store, mode, model_path);
    std::process::exit(exit_code);
}

fn run(
    command: Command,
    store: Arc<Mutex<Store>>,
    mode: OutputMode,
    model_path: Option<std::path::PathBuf>,
) -> i32 {
    match command {
        Command::Remember {
            agent,
            scope,
            role,
            dry_run,
            supersedes,
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
                // The supersede target is NOT verified here — the plan is a
                // preview, not a transaction.
                let plan = serde_json::json!({
                    "actions": [{
                        "action": "remember",
                        "agent": agent,
                        "scope": scope,
                        "role": role,
                        "supersedes": supersedes,
                        "content": content,
                    }],
                    "summary": format!(
                        "Would store 1 '{role}' memory ({} chars) in scope '{scope}'{}",
                        content.chars().count(),
                        match &supersedes {
                            Some(id) => format!(", superseding {id} (target not verified)"),
                            None => String::new(),
                        }
                    ),
                });
                emit_ok(
                    Response::new("engram memory remember --dry-run", plan).with_dry_run(),
                    mode,
                );
                return 0;
            }
            let guard = store.lock().expect("store lock poisoned");
            match supersedes {
                Some(target) => {
                    match guard.remember_superseding(&agent, &scope, &role, &content, &target) {
                        Ok(result) => match result.outcome {
                            store::SupersedeOutcome::Superseded => {
                                #[cfg(feature = "vector")]
                                if let Some(mem) = result.memory.as_ref() {
                                    try_embed_after_write(&guard, model_path.as_deref(), mem);
                                }
                                emit_ok(Response::new("engram memory remember", result), mode);
                                0
                            }
                            store::SupersedeOutcome::NotFound => fail(
                                AppError::new(
                                    error::ErrorCode::NotFound,
                                    3,
                                    format!("no memory '{target}' in scope '{scope}' (supersession is scope-local)"),
                                    format!("list ids with `engram recall --scope {scope}`"),
                                ),
                                mode,
                            ),
                            store::SupersedeOutcome::TargetIsRule => fail(
                                AppError::new(
                                    error::ErrorCode::InvalidArgument,
                                    2,
                                    format!("'{target}' is a rule; rules are never superseded here"),
                                    "rules have their own lifecycle: `engram rule add` revises, `engram rule retire` withdraws",
                                ),
                                mode,
                            ),
                            store::SupersedeOutcome::AlreadySuperseded => fail(
                                AppError::new(
                                    error::ErrorCode::Conflict,
                                    5,
                                    format!(
                                        "'{target}' was already superseded by '{}'",
                                        result.superseded_by_existing.as_deref().unwrap_or("unknown")
                                    ),
                                    "supersede the current winner instead, or recall --include-superseded to inspect the chain",
                                ),
                                mode,
                            ),
                        },
                        Err(e) => fail(AppError::from(e), mode),
                    }
                }
                None => match guard.remember(&agent, &scope, &role, &content) {
                    Ok(mem) => {
                        #[cfg(feature = "vector")]
                        try_embed_after_write(&guard, model_path.as_deref(), &mem);
                        emit_ok(Response::new("engram memory remember", mem), mode);
                        0
                    }
                    Err(e) => fail(AppError::from(e), mode),
                },
            }
        }
        Command::Recall {
            scope,
            limit,
            budget_tokens,
            as_of,
            include_superseded,
        } => {
            let validity = match resolve_validity(as_of.as_deref(), include_superseded) {
                Ok(v) => v,
                Err(e) => return fail(e, mode),
            };
            let guard = store.lock().expect("store lock poisoned");
            match guard.recall(&scope, limit, validity) {
                Ok(mems) => {
                    match budget_tokens {
                        Some(budget) => {
                            let (mems, report) = retrieval::budget_recall(mems, budget);
                            emit_ok(
                                Response::new("engram memory recall", mems).with_budget(report),
                                mode,
                            );
                        }
                        // No budget requested: byte-identical to the
                        // pre-budgeting output (no budget field serialized).
                        None => emit_ok(Response::new("engram memory recall", mems), mode),
                    }
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
            budget_tokens,
            as_of,
            include_superseded,
            mode: search_mode,
        } => {
            let validity = match resolve_validity(as_of.as_deref(), include_superseded) {
                Ok(v) => v,
                Err(e) => return fail(e, mode),
            };
            let guard = store.lock().expect("store lock poisoned");
            // Effective retrieval mode: an explicit --mode wins; otherwise
            // hybrid runs exactly when the gate passes (vector feature +
            // resolvable model + indexed vectors), else fts.
            let hybrid: Option<embed::HybridReady> = match search_mode {
                Some(cli::SearchMode::Fts) => None,
                Some(cli::SearchMode::Hybrid) => {
                    match embed::try_hybrid(&guard, model_path.as_deref(), &query) {
                        Ok(ready) => Some(ready),
                        Err(unavailable) => {
                            return fail(
                                AppError::new(
                                    error::ErrorCode::InvalidArgument,
                                    2,
                                    unavailable.message(),
                                    unavailable.hint(),
                                ),
                                mode,
                            )
                        }
                    }
                }
                None => embed::try_hybrid(&guard, model_path.as_deref(), &query).ok(),
            };
            match hybrid {
                Some(ready) => {
                    match guard.search_hybrid(
                        &query,
                        &ready.query_vec,
                        &ready.model,
                        scope.as_deref(),
                        limit,
                        validity,
                    ) {
                        Ok(hybrid) => {
                            match budget_tokens {
                                Some(budget) => {
                                    let (mems, mut report) =
                                        retrieval::budget_search(hybrid.memories, budget);
                                    // budget_search knows one channel; hybrid
                                    // fed three, so name them all with their
                                    // real candidate counts.
                                    report.channels = std::collections::BTreeMap::from([
                                        ("fts", hybrid.fts_candidates),
                                        ("vector", hybrid.vector_candidates),
                                        ("facts", hybrid.facts_candidates),
                                    ]);
                                    emit_ok(
                                        Response::new("engram memory search", mems)
                                            .with_budget(report),
                                        mode,
                                    );
                                }
                                None => emit_ok(
                                    Response::new("engram memory search", hybrid.memories),
                                    mode,
                                ),
                            }
                            0
                        }
                        Err(e) => fail(AppError::from(e), mode),
                    }
                }
                None => match guard.search(&query, scope.as_deref(), limit, validity) {
                    Ok(mems) => {
                        match budget_tokens {
                            Some(budget) => {
                                let (mems, report) = retrieval::budget_search(mems, budget);
                                emit_ok(
                                    Response::new("engram memory search", mems).with_budget(report),
                                    mode,
                                );
                            }
                            None => emit_ok(Response::new("engram memory search", mems), mode),
                        }
                        0
                    }
                    Err(e) => {
                        let err = AppError::from(e);
                        emit_error(&err, mode);
                        err.exit_code
                    }
                },
            }
        }
        Command::Context {
            scope,
            query,
            budget_tokens,
            limit,
        } => {
            let resolved = match rules::resolve_scope(scope.as_deref()) {
                Ok(r) => r,
                Err(e) => return fail(scope_error(e), mode),
            };
            let guard = store.lock().expect("store lock poisoned");
            // Same auto rule as search: the vector channel joins only when
            // the gate passes; any miss silently keeps the two-channel path.
            let ready: Option<embed::HybridReady> = query
                .as_deref()
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .and_then(|q| embed::try_hybrid(&guard, model_path.as_deref(), q).ok());
            let vector = ready.as_ref().map(|r| store::HybridQuery {
                model: &r.model,
                query_vec: &r.query_vec,
            });
            match guard.context(
                &resolved.name,
                query.as_deref(),
                limit,
                budget_tokens,
                vector,
            ) {
                Ok(ctx) => {
                    let report = ctx.budget.clone();
                    let result = ContextCommandResult {
                        scope: resolved.name,
                        scope_origin: resolved.origin,
                        context: ctx,
                    };
                    emit_ok(
                        Response::new("engram context", result).with_budget(report),
                        mode,
                    );
                    0
                }
                Err(e) => fail(AppError::from(e), mode),
            }
        }
        Command::Consolidate {
            extract,
            scope,
            dry_run,
        } => {
            if !extract {
                // At M4 extraction is the only consolidation phase, and an
                // explicit flag keeps the surface honest when M5 adds more.
                return fail(
                    AppError::new(
                        error::ErrorCode::InvalidArgument,
                        2,
                        "consolidate needs a phase flag",
                        "pass --extract (dedup/decay arrive at M5)",
                    ),
                    mode,
                );
            }
            let guard = store.lock().expect("store lock poisoned");
            // No scope cascade here, unlike the rule commands: None means
            // EVERY scope, because idle-time maintenance naturally spans
            // the whole database.
            match guard.facts_extract(scope.as_deref(), dry_run) {
                Ok(report) => {
                    let resp = if dry_run {
                        Response::new("engram consolidate --extract --dry-run", report)
                            .with_dry_run()
                    } else {
                        Response::new("engram consolidate --extract", report)
                    };
                    emit_ok(resp, mode);
                    0
                }
                Err(e) => fail(AppError::from(e), mode),
            }
        }
        Command::Rule { action } => run_rule(action, &store, mode),
        Command::Index {
            scope,
            batch,
            dry_run,
        } => run_index(scope, batch, dry_run, model_path, &store, mode),
        Command::Mcp => {
            // Bind the CLI --model-path into the process-wide embedder cache
            // before any request arrives; auto-hybrid then works server-side.
            #[cfg(feature = "vector")]
            embed::warm(model_path.as_deref());
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(e) = rt.block_on(mcp::run_stdio(store)) {
                eprintln!("{{\"error\":{{\"message\":\"{e}\"}}}}");
                return 1;
            }
            0
        }
        Command::Serve { addr } => {
            #[cfg(feature = "vector")]
            embed::warm(model_path.as_deref());
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
                    "remember", "recall", "search", "context", "index", "consolidate",
                    "rule add", "rule list", "rule retire", "rule sync", "rule purge",
                    "save-chat", "mcp", "serve", "schema", "describe"
                ],
                "facts": {
                    "extractor": facts::EXTRACTOR,
                    "philosophy": "facts are VERBATIM marker-prefixed lines/sentences (Decided:, \
                                   TODO, Never..., 19 markers) indexed with drill-down pointers to \
                                   their parent memories; extraction is deterministic — no LLM ever \
                                   runs on the write path, and a fact's liveness derives from its \
                                   parent memory's validity at query time",
                    "cli_only": true
                },
                "vector": {
                    "description": "Semantic (hybrid) retrieval: FTS5 + Model2Vec vector channels \
                                    fused with reciprocal rank fusion. Feature-gated at build time; \
                                    `engram index` backfills embeddings; models are loaded from a \
                                    local directory only — engram never downloads one.",
                    "feature_enabled": cfg!(feature = "vector"),
                    "model_cascade": ["--model-path", "ENGRAM_MODEL", "$XDG_DATA_HOME/engram/model (default ~/.local/share/engram/model)"],
                    "search_modes": ["fts", "hybrid"],
                    "auto_hybrid": "search/context use hybrid automatically when the feature is \
                                    built in AND a model resolves AND vector_count(model) > 0; \
                                    otherwise fts. Explicit --mode hybrid errors (exit 2) when a \
                                    prerequisite is missing.",
                    "index": "engram index [--scope S] [--batch N=500] [--dry-run] — embeds every \
                              memory the resolved model has not indexed yet (rules skipped); \
                              MCP/HTTP writes are picked up here, they never embed live"
                },
                "supersession": {
                    "description": "remember --supersedes closes the target's validity window \
                                    (valid_to + superseded_by) instead of deleting; reads default \
                                    to currently-valid rows.",
                    "scope_local": true,
                    "escape_hatches": ["--include-superseded", "--as-of <ISO8601>"],
                    "rule_purge": "CLI-only; deletes retired-rule tombstones only"
                },
                "transports": ["cli", "mcp-stdio", "http"],
                "storage": "sqlite+fts5, single shared file",
                "output": {
                    "formats": ["json", "jsonl", "csv"],
                    "jsonl": "first line is the metadata envelope with data:null, then one line per record",
                    "csv": "RFC 4180 data rows on stdout; metadata as one JSON line on stderr",
                    "dry_run": "remember and rule sync accept --dry-run; the envelope carries metadata.dry_run=true",
                    "accessible": "--accessible or SPACECRAFT_A11Y=1 (Standard §18); status tags [OK]/[ERROR] are present in every human mode",
                    "budgeting": "recall/search accept --budget-tokens and context always packs to one; \
                                  metadata.budget reports estimator 'chars-div-4', included/dropped counts, \
                                  dropped_ids, and per-channel candidate counts"
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
            // A chat archive wants the full verbatim history, superseded
            // rows included — save-chat is a record, not a context view.
            match guard.recall(&scope, u32::MAX, store::Validity::All) {
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
struct ContextCommandResult {
    scope: String,
    scope_origin: rules::ScopeOrigin,
    /// Flattened, so `data.rules` / `data.memories` / `data.budget` sit next
    /// to the resolved scope — same shape as `GET /v1/context`.
    #[serde(flatten)]
    context: store::ContextResult,
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

        RuleAction::Purge {
            id,
            scope,
            yes,
            dry_run,
        } => {
            let resolved = match rules::resolve_scope(scope.as_deref()) {
                Ok(r) => r,
                Err(e) => return fail(scope_error(e), mode),
            };
            let guard = store.lock().expect("store lock poisoned");
            // Classify first so --dry-run and the --yes gate both report the
            // real outcome without touching the row.
            let listed = match guard.rules_including_retired(&resolved.name) {
                Ok(r) => r,
                Err(e) => return fail(AppError::from(e), mode),
            };
            let target = listed.into_iter().find(|r| r.rule_id == id);
            match target {
                None => fail(
                    AppError::new(
                        error::ErrorCode::NotFound,
                        3,
                        format!("no rule '{id}' in scope '{}'", resolved.name),
                        "list the ids with `engram rule list --include-retired`",
                    ),
                    mode,
                ),
                Some(rule) if !rule.retired => fail(
                    AppError::new(
                        error::ErrorCode::InvalidArgument,
                        2,
                        format!("rule '{id}' is still active; purge deletes tombstones only"),
                        format!("retire it first: engram rule purge needs `engram rule retire --id {id}` before it"),
                    ),
                    mode,
                ),
                Some(rule) => {
                    if dry_run {
                        let plan = serde_json::json!({
                            "actions": [{
                                "action": "purge-rule",
                                "rule_id": rule.rule_id,
                                "scope": resolved.name,
                                "text": rule.text,
                            }],
                            "summary": format!(
                                "Would permanently delete retired rule '{id}' from scope '{}'",
                                resolved.name
                            ),
                        });
                        emit_ok(
                            Response::new("engram rule purge --dry-run", plan).with_dry_run(),
                            mode,
                        );
                        return 0;
                    }
                    if !yes {
                        // Wizard Fallback: this process may not have a TTY to
                        // ask on, so consent must arrive as a flag.
                        return fail(
                            AppError::new(
                                error::ErrorCode::InvalidArgument,
                                2,
                                "refusing to purge without --yes",
                                format!("engram rule purge --id {id} --scope {} --yes", resolved.name),
                            ),
                            mode,
                        );
                    }
                    match guard.rule_purge(&resolved.name, &id) {
                        Ok(outcome) => {
                            let result = serde_json::json!({
                                "rule_id": id,
                                "scope": resolved.name,
                                "scope_origin": resolved.origin,
                                "outcome": outcome,
                                "purged_text": rule.text,
                            });
                            emit_ok(Response::new("engram rule purge", result), mode);
                            0
                        }
                        Err(e) => fail(AppError::from(e), mode),
                    }
                }
            }
        }
    }
}

/// `engram index` — backfill embeddings for memories the resolved model has
/// not indexed yet. Vector-feature builds only; see the sibling stub below
/// for the structured refusal in a default build.
#[cfg(feature = "vector")]
fn run_index(
    scope: Option<String>,
    batch: u32,
    dry_run: bool,
    model_path: Option<std::path::PathBuf>,
    store: &Arc<Mutex<Store>>,
    mode: OutputMode,
) -> i32 {
    let Some(path) = embed::resolve_model_path(model_path.as_deref()) else {
        return fail(
            AppError::new(
                error::ErrorCode::InvalidArgument,
                2,
                "no embedding model found",
                "the model path cascade is: --model-path, then ENGRAM_MODEL, then \
                 $XDG_DATA_HOME/engram/model (default ~/.local/share/engram/model); none exists — \
                 place a Model2Vec model there yourself (engram never downloads anything)",
            ),
            mode,
        );
    };
    let embedder = match embed::Embedder::load(&path) {
        Ok(e) => e,
        Err(e) => {
            return fail(
                AppError::new(
                    error::ErrorCode::InvalidArgument,
                    2,
                    format!(
                        "failed to load the embedding model at {}: {e}",
                        path.display()
                    ),
                    "a Model2Vec model directory contains model.safetensors, tokenizer.json, \
                     and config.json",
                ),
                mode,
            )
        }
    };
    let model = embedder.model_name().to_string();
    let guard = store.lock().expect("store lock poisoned");

    if dry_run {
        // Count-only pass. Materializing the rows to count them is fine at
        // engram's scale (the same no-index reasoning as brute-force cosine).
        let pending = match guard.unindexed_memories(&model, scope.as_deref(), u32::MAX) {
            Ok(p) => p,
            Err(e) => return fail(AppError::from(e), mode),
        };
        let data = serde_json::json!({
            "model": model,
            "dim": embedder.dim(),
            "indexed": 0,
            "remaining": pending.len(),
        });
        emit_ok(
            Response::new("engram index --dry-run", data).with_dry_run(),
            mode,
        );
        return 0;
    }

    let mut indexed: u64 = 0;
    loop {
        // Each embedded batch leaves the work queue, so the loop always
        // fetches fresh work and terminates when the queue drains.
        let batch_mems = match guard.unindexed_memories(&model, scope.as_deref(), batch.max(1)) {
            Ok(b) => b,
            Err(e) => return fail(AppError::from(e), mode),
        };
        if batch_mems.is_empty() {
            break;
        }
        for mem in &batch_mems {
            let vec = embedder.embed(&mem.content);
            if let Err(e) = guard.vector_upsert(&mem.id, &model, &vec) {
                return fail(AppError::from(e), mode);
            }
            indexed += 1;
        }
    }
    let remaining = match guard.unindexed_memories(&model, scope.as_deref(), u32::MAX) {
        Ok(p) => p.len(),
        Err(e) => return fail(AppError::from(e), mode),
    };
    let data = serde_json::json!({
        "model": model,
        "dim": embedder.dim(),
        "indexed": indexed,
        "remaining": remaining,
    });
    emit_ok(Response::new("engram index", data), mode);
    0
}

/// `engram index` in a build without the vector feature: the command parses
/// (schema stability across builds), then refuses with a structured error.
#[cfg(not(feature = "vector"))]
fn run_index(
    _scope: Option<String>,
    _batch: u32,
    _dry_run: bool,
    _model_path: Option<std::path::PathBuf>,
    _store: &Arc<Mutex<Store>>,
    mode: OutputMode,
) -> i32 {
    fail(
        AppError::new(
            error::ErrorCode::InvalidArgument,
            2,
            "engram was built without the vector feature",
            "rebuild with `cargo build --release --features vector`",
        ),
        mode,
    )
}

/// Best-effort live embedding after a successful CLI `remember`.
///
/// Strictly fire-and-forget: a missing model, a load failure, or a write
/// error must never fail (or even annotate) the remember — the memory is
/// stored either way and `engram index` backfills whatever this skipped.
/// CLI-only by design this milestone: MCP/HTTP keep their lock holds short
/// and rely on the index command instead.
#[cfg(feature = "vector")]
fn try_embed_after_write(store: &Store, model_path: Option<&std::path::Path>, mem: &store::Memory) {
    let Some(embedder) = embed::try_embedder(model_path) else {
        return;
    };
    let vec = embedder.embed(&mem.content);
    let _ = store.vector_upsert(&mem.id, embedder.model_name(), &vec);
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

/// Maps the `--as-of` / `--include-superseded` pair to a [`store::Validity`],
/// validating the timestamp shape (§14: ISO 8601 UTC). clap's
/// `conflicts_with` already rules out both being set on the CLI.
#[expect(
    clippy::result_large_err,
    reason = "the Err is a one-shot CLI-argument failure on a cold path; boxing AppError would ripple through every handler for no measurable gain"
)]
fn resolve_validity(
    as_of: Option<&str>,
    include_superseded: bool,
) -> Result<store::Validity<'_>, AppError> {
    match as_of {
        Some(t) => {
            if t.parse::<jiff::Timestamp>().is_err() {
                return Err(AppError::new(
                    error::ErrorCode::InvalidArgument,
                    2,
                    format!("--as-of '{t}' is not an ISO 8601 UTC timestamp"),
                    "use the form 2026-08-01T12:00:00Z",
                ));
            }
            Ok(store::Validity::AsOf(t))
        }
        None if include_superseded => Ok(store::Validity::All),
        None => Ok(store::Validity::Current),
    }
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
