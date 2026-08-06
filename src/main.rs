// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Engram — shared verbatim chat memory for multi-model LLM pipelines.
//! Maintained by Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
//! https://Engram.SpacecraftSoftware.org/

mod archive;
mod cli;
mod embed;
mod error;
mod facts;
mod harness;
mod http;
mod install;
mod managed_file;
mod mcp;
mod output;
mod retrieval;
mod rules;
mod store;
mod time;
mod transcript;

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
        Ok(mut s) => {
            // Global --no-track: read-only auditing, wired before the store
            // is shared. CLI-only — the MCP/HTTP surfaces expose no opt-out
            // of their own (agent reads are what the tracking measures).
            if cli.no_track {
                s.set_tracking(false);
            }
            Arc::new(Mutex::new(s))
        }
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
            dedup,
            report,
            scope,
            dry_run,
            yes,
        } => {
            if !extract && !dedup && !report {
                // An explicit phase flag keeps the surface honest: silently
                // running every phase would make --yes far too easy to aim.
                return fail(
                    AppError::new(
                        error::ErrorCode::InvalidArgument,
                        2,
                        "consolidate needs at least one phase flag",
                        "pass --extract (fact extraction), --dedup (near-duplicate detection; \
                         add --yes to supersede), and/or --report (contradictions + decay, \
                         report-only)",
                    ),
                    mode,
                );
            }
            let guard = store.lock().expect("store lock poisoned");
            // No scope cascade here, unlike the rule commands: None means
            // EVERY scope, because idle-time maintenance naturally spans
            // the whole database.
            let mut sections = ConsolidateOutput {
                extract: None,
                dedup: None,
                report: None,
            };
            let mut command = String::from("engram consolidate");
            if extract {
                command.push_str(" --extract");
                match guard.facts_extract(scope.as_deref(), dry_run) {
                    Ok(r) => sections.extract = Some(r),
                    Err(e) => return fail(AppError::from(e), mode),
                }
            }
            if dedup {
                command.push_str(" --dedup");
                if yes {
                    command.push_str(" --yes");
                }
                // The cosine detector joins exactly when the auto-hybrid
                // gate would pass: vector feature + resolvable model +
                // indexed vectors. Otherwise the exact-text detector runs
                // alone.
                let vector_model = dedup_vector_model(&guard, model_path.as_deref());
                match guard.consolidate_dedup(scope.as_deref(), vector_model.as_deref(), yes) {
                    Ok(r) => sections.dedup = Some(r),
                    Err(e) => return fail(AppError::from(e), mode),
                }
            }
            if report {
                command.push_str(" --report");
                match guard.consolidate_report(scope.as_deref()) {
                    Ok(r) => sections.report = Some(r),
                    Err(e) => return fail(AppError::from(e), mode),
                }
            }
            // --dry-run is meaningful only for --extract (the other phases
            // are read-only without --yes anyway); the envelope marker
            // follows the same rule.
            let resp = if dry_run && extract {
                command.push_str(" --dry-run");
                Response::new(command, sections).with_dry_run()
            } else {
                Response::new(command, sections)
            };
            emit_ok(resp, mode);
            0
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
                "mcp": {
                    "tools": ["remember", "recall", "search", "get", "context",
                              "rule_add", "rule_list", "rule_retire", "rule_sync", "save_chat"],
                    "ceiling": 10,
                    "ceiling_reached": true,
                    "note": "every tool's schema costs context on every turn, so the surface is \
                             capped at ten; an eleventh must displace an existing one. save_chat \
                             takes no file argument: the destination is derived server-side.",
                    "cli_only": ["install", "ingest", "consolidate", "index", "rule purge"]
                },
                "commands": [
                    "remember", "recall", "search", "context", "index", "consolidate",
                    "rule add", "rule list", "rule retire", "rule sync", "rule purge",
                    "save-chat", "ingest", "mcp", "serve", "schema", "describe"
                ],
                "ingest": {
                    "purpose": "capture a harness's own session transcript into a scope as \
                                ordinary memories, so recall/search/context/consolidate see the \
                                real conversation rather than only what an agent chose to record",
                    "harnesses": harness::ALL.iter().map(|h| serde_json::json!({
                        "name": h.name,
                        "reader": matches!(h.transcript, harness::TranscriptSupport::Reader(_)),
                    })).collect::<Vec<_>>(),
                    "excluded_by_default": ["tool_use", "tool_result", "thinking", "sidechains"],
                    "tool_payloads": "never stored, even with --include-tools: a tool result is \
                                      summarized to its size. Payloads are where file contents, \
                                      command output, and credentials live",
                    "redaction": "credential-shaped substrings are replaced before storage and \
                                  counted per kind in the response; best-effort, not a guarantee",
                    "idempotent": "turn ids are uuid v5 over (harness, session, record), so \
                                   re-ingesting a session inserts nothing and resuming one \
                                   inserts only the new tail",
                    "created_at": "the transcript's own timestamp, not the ingest time — recall \
                                   orders by created_at, so a wall-clock stamp would destroy the \
                                   conversation's reading order",
                    "unreadable_harness": "a structured exit-2 error naming the fallback, never \
                                           an empty success",
                    "cli_only": true
                },
                "consolidate": {
                    "phases": ["extract", "dedup", "report"],
                    "extract": "deterministic fact extraction into the facts index (honors --dry-run)",
                    "dedup": "near-duplicate groups per scope — normalized-exact text always, \
                              stored-vector cosine >= 0.92 when the hybrid gate passes; the \
                              newest row wins and --yes supersedes the losers (M2 semantics, \
                              never a delete); report-only without --yes",
                    "report": "always report-only: contradiction pairs (>= 0.5 word-set Jaccard \
                               with a negation marker on exactly one side — a heuristic; resolve \
                               via remember --supersedes, never auto-resolved) and the top 20 \
                               stalest memories by age_days + 30/(1+access_count)",
                    "cli_only": true
                },
                "access_tracking": {
                    "columns": ["access_count", "last_accessed_at"],
                    "opt_out": "--no-track (CLI)",
                    "semantics": "recall/search/context/get bump the counters for the memories \
                                  actually returned (not candidates); the columns are internal — \
                                  Memory output never carries them — and feed the decay report; \
                                  MCP/HTTP reads always track"
                },
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
        Command::SaveChat {
            scope,
            file,
            model,
            dry_run,
        } => {
            // The archive lands at the project root, not wherever the process
            // happened to start: a harness slash command runs from an
            // arbitrary subdirectory, and `chat/` belongs in exactly one place.
            let resolved = match rules::resolve_scope(Some(&scope)) {
                Ok(r) => r,
                Err(e) => return fail(scope_error(e), mode),
            };

            let mems = {
                let guard = store.lock().expect("store lock poisoned");
                match guard.export_history(&resolved.name) {
                    Ok(m) => m,
                    Err(e) => return fail(AppError::from(e), mode),
                }
            };
            if mems.is_empty() {
                return fail(
                    AppError::new(
                        error::ErrorCode::NotFound,
                        3,
                        format!("no memories found for scope '{}'", resolved.name),
                        "verify the scope name is correct",
                    ),
                    mode,
                );
            }

            match archive::save_chat(&resolved, mems, file, model, dry_run) {
                Ok(result) => {
                    let resp = Response::new("engram save-chat", result);
                    let resp = if dry_run { resp.with_dry_run() } else { resp };
                    emit_ok(resp, mode);
                    0
                }
                Err(e) => fail(
                    AppError::new(
                        error::ErrorCode::InternalError,
                        1,
                        format!("failed to save chat: {}", e),
                        "check filesystem permissions and directory configuration",
                    ),
                    mode,
                ),
            }
        }

        Command::Ingest {
            harness: requested,
            session,
            scope,
            cwd,
            include_thinking,
            include_tools,
            include_sidechains,
            max_bytes,
            max_chars_per_turn,
            list,
            dry_run,
        } => {
            let opts = transcript::ReadOptions {
                include_thinking,
                include_tools,
                include_sidechains,
                max_bytes,
                max_chars_per_turn,
            };
            handle_ingest(
                &store, mode, requested, &session, scope, cwd, opts, list, dry_run,
            )
        }

        Command::Install {
            harness: requested,
            db_path,
            list,
            dry_run,
            force,
            hooks,
        } => handle_install(
            mode,
            &requested,
            db_path.as_deref(),
            list,
            dry_run,
            force,
            hooks,
        ),
    }
}

/// `engram install --list` payload: what engram sees, before it writes.
#[derive(serde::Serialize)]
struct InstallListResult {
    harnesses: Vec<InstallCandidate>,
    /// Where the plugin lives in the source tree, for users who would rather
    /// reference it declaratively than have files written into their home.
    plugin_dir: String,
}

#[derive(serde::Serialize)]
struct InstallCandidate {
    #[serde(flatten)]
    detected: harness::Detected,
    /// `null` when the harness has no command surface engram can write.
    commands_dir: Option<String>,
    /// The database this harness already registered engram against.
    #[serde(skip_serializing_if = "Option::is_none")]
    registered_db: Option<String>,
}

fn handle_install(
    mode: OutputMode,
    requested: &[harness::Harness],
    db_override: Option<&str>,
    list: bool,
    dry_run: bool,
    force: bool,
    hooks: bool,
) -> i32 {
    if harness::home_dir().is_none() {
        return fail(
            AppError::new(
                error::ErrorCode::InvalidArgument,
                2,
                "cannot resolve the home directory",
                "set HOME to the account whose harnesses should be configured",
            ),
            mode,
        );
    }

    if list {
        let harnesses = harness::ALL
            .iter()
            .map(|spec| InstallCandidate {
                detected: harness::describe(spec),
                commands_dir: harness::commands_dir(spec).map(|p| p.to_string_lossy().into_owned()),
                registered_db: harness::registered_db(spec)
                    .map(|p| p.to_string_lossy().into_owned()),
            })
            .collect();
        emit_ok(
            Response::new(
                "engram install --list",
                InstallListResult {
                    harnesses,
                    plugin_dir: install::plugin_dir_hint().to_string_lossy().into_owned(),
                },
            ),
            mode,
        );
        return 0;
    }

    let targets: Vec<&'static harness::HarnessSpec> = if requested.is_empty() {
        install::default_targets()
    } else {
        requested.iter().map(|id| harness::spec(*id)).collect()
    };

    match install::install(&targets, db_override, force, dry_run, hooks) {
        Ok(result) => {
            let resp = Response::new("engram install", result);
            let resp = if dry_run { resp.with_dry_run() } else { resp };
            emit_ok(resp, mode);
            0
        }
        Err(e) => fail(
            AppError::new(
                error::ErrorCode::InternalError,
                1,
                format!("failed to install commands: {e}"),
                "check write permissions on the harness command directories",
            ),
            mode,
        ),
    }
}

/// `engram ingest --list` payload.
///
/// Carries the full harness table alongside the sessions found, so a user
/// looking at an empty list can tell "engram found no session here" from
/// "engram cannot read this harness at all" without running a second command.
#[derive(serde::Serialize)]
struct IngestListResult {
    harness: &'static str,
    cwd: String,
    sessions: Vec<transcript::SessionRef>,
    harnesses: Vec<harness::Detected>,
}

/// `engram ingest` payload: the shared capture summary plus where it ran.
#[derive(serde::Serialize)]
struct IngestResult {
    harness: &'static str,
    scope: String,
    scope_origin: rules::ScopeOrigin,
    cwd: String,
    dry_run: bool,
    /// Sessions, totals, filter histogram, and redaction counts — produced by
    /// [`transcript::capture`], which the MCP `save_chat` tool also calls, so
    /// the two surfaces cannot disagree about what was captured.
    #[serde(flatten)]
    capture: transcript::CaptureSummary,
}

/// Resolves the harness to read from.
///
/// Explicit `--harness` wins. Otherwise the environment marker of the harness
/// engram is running under, then — only if exactly one installed harness has
/// a reader — that one. Two candidates is an error, never a guess: ingesting
/// the wrong harness's transcript into a scope is not something a user can
/// easily undo.
/// The error is boxed because [`AppError`] is large and this `Result` is
/// returned on the common path; clippy's `result_large_err` is right that
/// paying for it on every success would be wasteful.
fn resolve_harness(
    requested: Option<harness::Harness>,
) -> Result<&'static harness::HarnessSpec, Box<AppError>> {
    if let Some(id) = requested {
        return Ok(harness::spec(id));
    }
    if let Some(spec) = harness::from_env() {
        return Ok(spec);
    }
    let readable = harness::readable_and_present();
    match readable.len() {
        1 => Ok(readable[0]),
        0 => Err(Box::new(AppError::new(
            error::ErrorCode::InvalidArgument,
            2,
            "no harness with a transcript reader is installed",
            format!(
                "engram can read transcripts from: {}. Pass --harness to name one explicitly.",
                readable_names()
            ),
        ))),
        _ => Err(Box::new(AppError::new(
            error::ErrorCode::InvalidArgument,
            2,
            "several harnesses are installed and engram will not guess between them",
            format!("pass --harness with one of: {}", readable_names()),
        ))),
    }
}

/// Comma-separated names of every harness engram can read from.
fn readable_names() -> String {
    harness::ALL
        .iter()
        .filter(|s| matches!(s.transcript, harness::TranscriptSupport::Reader(_)))
        .map(|s| s.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Turns a [`transcript::TranscriptError`] into a structured CLI error.
///
/// The no-reader case names the fallback explicitly. Reporting "0 sessions"
/// for a harness engram simply cannot read would be the silent-failure mode
/// this whole design exists to prevent.
fn transcript_error(e: transcript::TranscriptError, harness_name: &str) -> AppError {
    match e {
        transcript::TranscriptError::NoReader(detail) => AppError::new(
            error::ErrorCode::InvalidArgument,
            2,
            format!("harness '{harness_name}' has no transcript reader"),
            format!(
                "{detail}. Use the notes path instead: call `remember` during the session, \
                 then `engram save-chat --scope <id>`."
            ),
        ),
        transcript::TranscriptError::NoHome => AppError::new(
            error::ErrorCode::InvalidArgument,
            2,
            "cannot resolve the home directory",
            "set HOME to the account whose harness transcripts should be read",
        ),
        transcript::TranscriptError::TooLarge { bytes, max_bytes } => AppError::new(
            error::ErrorCode::InvalidArgument,
            2,
            format!("transcript is {bytes} bytes, above the {max_bytes}-byte ceiling"),
            "raise the ceiling with --max-bytes if this session really is that large",
        ),
        transcript::TranscriptError::BadTimestamp { record, value } => AppError::new(
            error::ErrorCode::InvalidArgument,
            2,
            format!("record {record} carries an unparseable timestamp '{value}'"),
            "engram will not substitute the current time: doing so would silently destroy the \
             conversation's reading order. Report this as a transcript-format change.",
        ),
        transcript::TranscriptError::Io(e) => AppError::new(
            error::ErrorCode::InternalError,
            1,
            format!("cannot read the transcript: {e}"),
            "check that the transcript file is readable",
        ),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "one flag per CLI option; grouping them into a struct would only \
              move the argument list one call further out"
)]
fn handle_ingest(
    store: &Arc<Mutex<Store>>,
    mode: OutputMode,
    requested: Option<harness::Harness>,
    session_selector: &str,
    scope: Option<String>,
    cwd: Option<std::path::PathBuf>,
    opts: transcript::ReadOptions,
    list: bool,
    dry_run: bool,
) -> i32 {
    let spec = match resolve_harness(requested) {
        Ok(s) => s,
        Err(e) => return fail(*e, mode),
    };

    let cwd = match cwd.map_or_else(std::env::current_dir, Ok) {
        Ok(c) => c,
        Err(e) => return fail(scope_error(e), mode),
    };

    let sessions = match transcript::sessions_for(spec, &cwd) {
        Ok(s) => s,
        Err(e) => return fail(transcript_error(e, spec.name), mode),
    };

    if list {
        emit_ok(
            Response::new(
                "engram ingest --list",
                IngestListResult {
                    harness: spec.name,
                    cwd: cwd.to_string_lossy().into_owned(),
                    sessions,
                    harnesses: harness::detect(),
                },
            ),
            mode,
        );
        return 0;
    }

    let selected: Vec<transcript::SessionRef> = match session_selector {
        "latest" => sessions.into_iter().take(1).collect(),
        "all" => sessions,
        id => sessions
            .into_iter()
            .filter(|s| s.session_id == id)
            .collect(),
    };
    if selected.is_empty() {
        return fail(
            AppError::new(
                error::ErrorCode::NotFound,
                3,
                format!(
                    "no {} session matching '{session_selector}' for {}",
                    spec.name,
                    cwd.display()
                ),
                "run `engram ingest --list` to see the sessions engram can find",
            ),
            mode,
        );
    }

    let resolved = match rules::resolve_scope(scope.as_deref()) {
        Ok(r) => r,
        Err(e) => return fail(scope_error(e), mode),
    };

    let capture = match transcript::capture(store, spec, &selected, &resolved.name, &opts, dry_run)
    {
        Ok(c) => c,
        Err(transcript::CaptureError::Transcript(e)) => {
            return fail(transcript_error(e, spec.name), mode)
        }
        Err(transcript::CaptureError::Storage(e)) => return fail(AppError::from(e), mode),
    };

    let result = IngestResult {
        harness: spec.name,
        scope: resolved.name,
        scope_origin: resolved.origin,
        cwd: cwd.to_string_lossy().into_owned(),
        dry_run,
        capture,
    };
    let resp = Response::new("engram ingest", result);
    let resp = if dry_run { resp.with_dry_run() } else { resp };
    emit_ok(resp, mode);
    0
}

/// `engram consolidate` payload: one optional section per phase that ran,
/// absent sections omitted entirely — the data shape mirrors the flags.
#[derive(serde::Serialize)]
struct ConsolidateOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    extract: Option<store::ExtractReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dedup: Option<store::DedupReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<store::ConsolidateReport>,
}

/// The model whose stored vectors the dedup cosine detector may compare —
/// `Some` exactly when the auto-hybrid gate would pass (vector feature
/// compiled in, a local model resolves and loads, and that model has
/// indexed vectors). Any miss means the exact-text detector runs alone; a
/// dedup never errors over a missing model.
#[cfg(feature = "vector")]
fn dedup_vector_model(store: &Store, model_path: Option<&std::path::Path>) -> Option<String> {
    let embedder = embed::try_embedder(model_path)?;
    let model = embedder.model_name().to_string();
    match store.vector_count(&model) {
        Ok(count) if count > 0 => Some(model),
        _ => None,
    }
}

/// Without the vector feature there are no stored vectors to compare; the
/// exact-text detector always runs alone.
#[cfg(not(feature = "vector"))]
fn dedup_vector_model(_store: &Store, _model_path: Option<&std::path::Path>) -> Option<String> {
    None
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
