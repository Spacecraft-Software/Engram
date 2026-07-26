// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! HTTP API. Same Store, same data shapes as the CLI and MCP surfaces —
//! for tools that don't speak MCP (curl, Nushell `http`, other services).
//! Local-only by default (PFA: bind 127.0.0.1, no auth, no telemetry).
//! Add a Bearer token check before exposing this beyond localhost.

use crate::output::envelope::Response;
use crate::store::{RetireOutcome, Store};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};

type SharedStore = Arc<Mutex<Store>>;

/// A response that carries a real HTTP status code.
///
/// The older `/v1/memory*` handlers below answer `200 OK` with an `{"error":...}`
/// body when they fail. That is wrong, but changing it would break existing
/// callers, so the rule routes use this instead of inheriting the bug. Treat
/// this as the pattern to follow when the memory routes are eventually fixed.
type ApiResult = (StatusCode, Json<serde_json::Value>);

fn ok(operation: &'static str, data: impl serde::Serialize) -> ApiResult {
    let body = serde_json::to_value(Response::new(operation, data))
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
    (StatusCode::OK, Json(body))
}

fn err(status: StatusCode, message: impl std::fmt::Display, hint: &str) -> ApiResult {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "message": message.to_string(),
                "hint": hint,
                "timestamp": crate::time::now_iso8601(),
            }
        })),
    )
}

pub fn router(store: SharedStore) -> Router {
    Router::new()
        .route("/v1/memory", post(remember))
        .route("/v1/memory/recall", get(recall))
        .route("/v1/memory/search", get(search))
        // Rules. DELETE maps to retire, which tombstones rather than erases —
        // documented on the handler, because the verb implies otherwise.
        .route("/v1/rules", post(rule_add).get(rule_list))
        .route("/v1/rules/sync", post(rule_sync))
        // `:rule_id` — axum 0.7 / matchit 0.7 path-parameter syntax. The `{rule_id}`
        // form is axum 0.8+; under 0.7 it compiles but matches only the literal
        // string, so the route silently never fires. Revisit on the 0.8 upgrade.
        .route("/v1/rules/:rule_id", delete(rule_retire))
        .route("/v1/health", get(health))
        .with_state(store)
}

#[derive(Deserialize)]
struct RememberBody {
    agent: String,
    scope: String,
    #[serde(default = "default_role")]
    role: String,
    content: String,
}
fn default_role() -> String { "note".to_string() }

async fn remember(
    State(store): State<SharedStore>,
    Json(body): Json<RememberBody>,
) -> Json<serde_json::Value> {
    let guard = store.lock().expect("store lock poisoned");
    match guard.remember(&body.agent, &body.scope, &body.role, &body.content) {
        Ok(mem) => Json(serde_json::to_value(Response::new("POST /v1/memory", mem)).unwrap()),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

#[derive(Deserialize)]
struct RecallParams {
    scope: String,
    #[serde(default = "default_limit")]
    limit: u32,
}
fn default_limit() -> u32 { 50 }

async fn recall(State(store): State<SharedStore>, Query(q): Query<RecallParams>) -> Json<serde_json::Value> {
    let guard = store.lock().expect("store lock poisoned");
    match guard.recall(&q.scope, q.limit) {
        Ok(mems) => Json(serde_json::to_value(Response::new("GET /v1/memory/recall", mems)).unwrap()),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

#[derive(Deserialize)]
struct SearchParams {
    query: String,
    scope: Option<String>,
    #[serde(default = "default_search_limit")]
    limit: u32,
}
fn default_search_limit() -> u32 { 20 }

async fn search(State(store): State<SharedStore>, Query(q): Query<SearchParams>) -> Json<serde_json::Value> {
    let guard = store.lock().expect("store lock poisoned");
    match guard.search(&q.query, q.scope.as_deref(), q.limit) {
        Ok(mems) => Json(serde_json::to_value(Response::new("GET /v1/memory/search", mems)).unwrap()),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

#[derive(Deserialize)]
struct RuleAddBody {
    rule_id: String,
    text: String,
    scope: Option<String>,
    #[serde(default = "default_http_agent")]
    agent: String,
}
fn default_http_agent() -> String { "http-client".to_string() }

/// `POST /v1/rules` — create or revise a rule. Re-using a `rule_id` updates
/// that rule in place and reinstates it if it was retired.
async fn rule_add(State(store): State<SharedStore>, Json(body): Json<RuleAddBody>) -> ApiResult {
    if let Err(reason) = crate::rules::validate_rule_id(&body.rule_id) {
        return err(StatusCode::BAD_REQUEST, reason, "use a short kebab-case id");
    }
    if let Err(reason) = crate::rules::validate_rule_text(&body.text) {
        return err(StatusCode::BAD_REQUEST, reason, "state the rule as plain prose");
    }
    let resolved = match crate::rules::resolve_scope(body.scope.as_deref()) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e, "pass \"scope\" explicitly"),
    };

    let guard = store.lock().expect("store lock poisoned");
    match guard.rule_add(&body.agent, &resolved.name, &body.rule_id, body.text.trim()) {
        Ok(upsert) => ok(
            "POST /v1/rules",
            serde_json::json!({
                "rule": upsert.rule,
                "created": upsert.created,
                "scope": resolved.name,
                "scope_origin": resolved.origin,
                "next_step": "POST /v1/rules/sync to render this rule into AGENTS.md and CLAUDE.md",
            }),
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e, "check that the database is writable"),
    }
}

#[derive(Deserialize)]
struct RuleListParams {
    scope: Option<String>,
    #[serde(default)]
    include_retired: bool,
}

/// `GET /v1/rules` — active rules for a scope, or all of them with
/// `?include_retired=true`.
async fn rule_list(State(store): State<SharedStore>, Query(q): Query<RuleListParams>) -> ApiResult {
    let resolved = match crate::rules::resolve_scope(q.scope.as_deref()) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e, "pass ?scope= explicitly"),
    };
    let guard = store.lock().expect("store lock poisoned");
    let listed = if q.include_retired {
        guard.rules_including_retired(&resolved.name)
    } else {
        guard.rules(&resolved.name)
    };
    match listed {
        Ok(rules) => ok(
            "GET /v1/rules",
            serde_json::json!({
                "scope": resolved.name,
                "scope_origin": resolved.origin,
                "count": rules.len(),
                "rules": rules,
            }),
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e, "check that the database is readable"),
    }
}

#[derive(Deserialize)]
struct RuleRetireParams {
    scope: Option<String>,
}

/// `DELETE /v1/rules/{rule_id}` — withdraw a rule.
///
/// Despite the verb this is a soft delete: the row is tombstoned so the record
/// of a policy that once applied survives, and `search` still reaches it.
/// Returns 404 when no such rule exists, and 200 with
/// `"outcome": "already-retired"` when it was already withdrawn.
async fn rule_retire(
    State(store): State<SharedStore>,
    Path(rule_id): Path<String>,
    Query(q): Query<RuleRetireParams>,
) -> ApiResult {
    let resolved = match crate::rules::resolve_scope(q.scope.as_deref()) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e, "pass ?scope= explicitly"),
    };
    let guard = store.lock().expect("store lock poisoned");
    match guard.rule_retire(&resolved.name, &rule_id) {
        Ok(retire) if retire.outcome == RetireOutcome::NotFound => err(
            StatusCode::NOT_FOUND,
            format!("no rule '{rule_id}' in scope '{}'", resolved.name),
            "GET /v1/rules?include_retired=true to list the ids",
        ),
        Ok(retire) => ok(
            "DELETE /v1/rules/:rule_id",
            serde_json::json!({
                "rule_id": retire.rule_id,
                "scope": retire.scope,
                "outcome": retire.outcome,
                "rule": retire.rule,
                "scope_origin": resolved.origin,
                "next_step": "POST /v1/rules/sync to drop this rule from AGENTS.md and CLAUDE.md",
            }),
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e, "check that the database is writable"),
    }
}

#[derive(Deserialize)]
struct RuleSyncBody {
    scope: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

/// `POST /v1/rules/sync` — render a scope's rules into `AGENTS.md` and
/// `CLAUDE.md` at the project root.
///
/// This is the only route that writes outside the database. The target paths
/// are derived from the server process's own working directory, never from
/// caller input, so there is no path-traversal surface — but it does mean an
/// unauthenticated local caller can rewrite those two files. The `--file`
/// override available on the CLI is deliberately not exposed here.
async fn rule_sync(State(store): State<SharedStore>, Json(body): Json<RuleSyncBody>) -> ApiResult {
    let resolved = match crate::rules::resolve_scope(body.scope.as_deref()) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e, "pass \"scope\" explicitly"),
    };
    let rule_list = {
        let guard = store.lock().expect("store lock poisoned");
        match guard.rules(&resolved.name) {
            Ok(r) => r,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e, "check that the database is readable"),
        }
    };

    let block = crate::rules::render_block(&resolved.name, &rule_list);
    let mut written = Vec::new();
    for path in crate::rules::target_paths(&resolved.root, &[]) {
        match crate::rules::sync_file(&path, &block, body.dry_run) {
            Ok(file) => written.push(file),
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to sync {}: {e}", path.display()),
                    "check filesystem permissions for the project root",
                )
            }
        }
    }

    ok(
        "POST /v1/rules/sync",
        serde_json::json!({
            "scope": resolved.name,
            "scope_origin": resolved.origin,
            "root": resolved.root.to_string_lossy(),
            "rule_count": rule_list.len(),
            "dry_run": body.dry_run,
            "files": written,
        }),
    )
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "timestamp": crate::time::now_iso8601() }))
}
