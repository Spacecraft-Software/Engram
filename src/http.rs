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
/// Every route uses this. (Before 0.2.0 the `/v1/memory*` handlers answered
/// `200 OK` with an `{"error":...}` body — that defect was fixed as a
/// deliberate breaking change; errors now carry 4xx/5xx status codes.)
type ApiResult = (StatusCode, Json<serde_json::Value>);

fn ok(operation: &'static str, data: impl serde::Serialize) -> ApiResult {
    ok_resp(Response::new(operation, data))
}

/// Like [`ok`], but for a caller-built [`Response`] — used when the envelope
/// needs decoration (e.g. `with_budget`) before serialization.
fn ok_resp<T: serde::Serialize>(resp: Response<T>) -> ApiResult {
    let body = serde_json::to_value(resp)
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
        .route("/v1/context", get(context))
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
fn default_role() -> String {
    "note".to_string()
}

async fn remember(State(store): State<SharedStore>, Json(body): Json<RememberBody>) -> ApiResult {
    if body.content.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "content must not be empty",
            "provide the verbatim message text in \"content\"",
        );
    }
    let guard = store.lock().expect("store lock poisoned");
    match guard.remember(&body.agent, &body.scope, &body.role, &body.content) {
        Ok(mem) => ok("POST /v1/memory", mem),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            e,
            "check that the database is writable",
        ),
    }
}

#[derive(Deserialize)]
struct RecallParams {
    scope: String,
    #[serde(default = "default_limit")]
    limit: u32,
    /// Pack results to a token budget (chars/4 estimate); the envelope
    /// carries `metadata.budget` when set.
    budget_tokens: Option<u32>,
}
fn default_limit() -> u32 {
    50
}

async fn recall(State(store): State<SharedStore>, Query(q): Query<RecallParams>) -> ApiResult {
    let guard = store.lock().expect("store lock poisoned");
    match guard.recall(&q.scope, q.limit) {
        Ok(mems) => match q.budget_tokens {
            Some(budget) => {
                let (mems, report) = crate::retrieval::budget_recall(mems, budget);
                ok_resp(Response::new("GET /v1/memory/recall", mems).with_budget(report))
            }
            None => ok("GET /v1/memory/recall", mems),
        },
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            e,
            "check that the database is readable",
        ),
    }
}

#[derive(Deserialize)]
struct SearchParams {
    query: String,
    scope: Option<String>,
    #[serde(default = "default_search_limit")]
    limit: u32,
    /// Pack results to a token budget (chars/4 estimate); the envelope
    /// carries `metadata.budget` when set.
    budget_tokens: Option<u32>,
}
fn default_search_limit() -> u32 {
    20
}

async fn search(State(store): State<SharedStore>, Query(q): Query<SearchParams>) -> ApiResult {
    let guard = store.lock().expect("store lock poisoned");
    match guard.search(&q.query, q.scope.as_deref(), q.limit) {
        Ok(mems) => match q.budget_tokens {
            Some(budget) => {
                let (mems, report) = crate::retrieval::budget_search(mems, budget);
                ok_resp(Response::new("GET /v1/memory/search", mems).with_budget(report))
            }
            None => ok("GET /v1/memory/search", mems),
        },
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            e,
            "check that the database is readable",
        ),
    }
}

#[derive(Deserialize)]
struct ContextParams {
    /// Optional; resolved through the same cascade as the rule routes
    /// (`ENGRAM_SCOPE`, git working-tree basename, cwd basename).
    scope: Option<String>,
    query: Option<String>,
    #[serde(default = "default_budget_tokens")]
    budget_tokens: u32,
    #[serde(default = "default_limit")]
    limit: u32,
}
fn default_budget_tokens() -> u32 {
    3000
}

/// `GET /v1/context` — a budget-packed context block for session start:
/// active rules first (always included, even over budget), then
/// recency+relevance fused memories. Same data shape as `engram context`.
async fn context(State(store): State<SharedStore>, Query(q): Query<ContextParams>) -> ApiResult {
    let resolved = match crate::rules::resolve_scope(q.scope.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                e,
                "pass ?scope= explicitly",
            )
        }
    };
    let guard = store.lock().expect("store lock poisoned");
    match guard.context(&resolved.name, q.query.as_deref(), q.limit, q.budget_tokens) {
        Ok(ctx) => {
            let report = ctx.budget.clone();
            let data = serde_json::json!({
                "scope": resolved.name,
                "scope_origin": resolved.origin,
                "rules": ctx.rules,
                "memories": ctx.memories,
                "rules_tokens": ctx.rules_tokens,
                "budget": ctx.budget,
            });
            ok_resp(Response::new("GET /v1/context", data).with_budget(report))
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            e,
            "check that the database is readable",
        ),
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
fn default_http_agent() -> String {
    "http-client".to_string()
}

/// `POST /v1/rules` — create or revise a rule. Re-using a `rule_id` updates
/// that rule in place and reinstates it if it was retired.
async fn rule_add(State(store): State<SharedStore>, Json(body): Json<RuleAddBody>) -> ApiResult {
    if let Err(reason) = crate::rules::validate_rule_id(&body.rule_id) {
        return err(StatusCode::BAD_REQUEST, reason, "use a short kebab-case id");
    }
    if let Err(reason) = crate::rules::validate_rule_text(&body.text) {
        return err(
            StatusCode::BAD_REQUEST,
            reason,
            "state the rule as plain prose",
        );
    }
    let resolved = match crate::rules::resolve_scope(body.scope.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                e,
                "pass \"scope\" explicitly",
            )
        }
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
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            e,
            "check that the database is writable",
        ),
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
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                e,
                "pass ?scope= explicitly",
            )
        }
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
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            e,
            "check that the database is readable",
        ),
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
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                e,
                "pass ?scope= explicitly",
            )
        }
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
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            e,
            "check that the database is writable",
        ),
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
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                e,
                "pass \"scope\" explicitly",
            )
        }
    };
    let rule_list = {
        let guard = store.lock().expect("store lock poisoned");
        match guard.rules(&resolved.name) {
            Ok(r) => r,
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    e,
                    "check that the database is readable",
                )
            }
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

// ---------------------------------------------------------------------------
// Tests. The router and `Store` live in this binary crate (there is no lib
// target), so the HTTP surface is exercised here with
// `tower::ServiceExt::oneshot` instead of from `tests/` — every request goes
// through real routing, extraction, and serialization, but no socket is bound.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// Opens a fresh `Store` on a throwaway SQLite file in its own temporary
    /// directory.
    ///
    /// The `TempDir` guard must stay bound in the test (`let (_dir, store) =
    /// ...`) — dropping it deletes the database along with its WAL/SHM
    /// sidecar files. A directory per test keeps concurrently running tests
    /// fully isolated from one another.
    fn test_store() -> (tempfile::TempDir, SharedStore) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let store = Store::open(&dir.path().join("engram-test.db")).expect("open test store");
        (dir, Arc::new(Mutex::new(store)))
    }

    /// Drives one request through a clone of the router; returns the status
    /// and the raw body text. Raw text rather than parsed JSON because not
    /// every body is JSON — axum's own extractor rejections are plain text.
    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let response = app
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("read response body")
            .to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Parses a handler-produced body, panicking with the body text in the
    /// message so a failing test shows exactly what came back.
    fn parse(body: &str) -> serde_json::Value {
        serde_json::from_str(body).unwrap_or_else(|e| panic!("body is not JSON ({e}): {body}"))
    }

    fn post_json(uri: &str, body: &serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("request builds")
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request builds")
    }

    fn delete(uri: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .body(Body::empty())
            .expect("request builds")
    }

    #[tokio::test]
    async fn post_memory_round_trips_content() {
        let (_dir, store) = test_store();
        let app = router(store);
        let body = serde_json::json!({
            "agent": "test-agent",
            "scope": "http-remember",
            "content": "The reactor stays at 80 percent.",
        });
        let (status, text) = send(&app, post_json("/v1/memory", &body)).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        let json = parse(&text);
        assert_eq!(
            json["metadata"]["tool"], "engram",
            "envelope metadata missing: {json}"
        );
        assert!(json["data"]["id"].is_string(), "data.id missing: {json}");
        assert_eq!(json["data"]["content"], "The reactor stays at 80 percent.");
        assert_eq!(json["data"]["role"], "note", "role should default to note");
    }

    #[tokio::test]
    async fn post_memory_whitespace_content_is_400() {
        let (_dir, store) = test_store();
        let app = router(store);
        let body = serde_json::json!({
            "agent": "test-agent",
            "scope": "http-remember",
            "content": "   \n\t",
        });
        let (status, text) = send(&app, post_json("/v1/memory", &body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {text}");
        let json = parse(&text);
        let message = json["error"]["message"]
            .as_str()
            .expect("error.message present");
        assert!(
            message.contains("content"),
            "message should name the field: {message}"
        );
    }

    #[tokio::test]
    async fn post_memory_malformed_json_is_client_error() {
        let (_dir, store) = test_store();
        let app = router(store);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/memory")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{\"agent\": \"a\", "))
            .expect("request builds");
        let (status, text) = send(&app, req).await;
        assert!(
            status.is_client_error(),
            "expected 4xx for malformed JSON, got {status}: {text}"
        );
    }

    #[tokio::test]
    async fn recall_returns_chronological_order() {
        let (_dir, store) = test_store();
        let app = router(store);
        for content in ["first message", "second message"] {
            let body = serde_json::json!({
                "agent": "test-agent",
                "scope": "recall-order",
                "content": content,
            });
            let (status, text) = send(&app, post_json("/v1/memory", &body)).await;
            assert_eq!(status, StatusCode::OK, "insert failed: {text}");
            // Recall orders by `created_at`; a short pause guarantees the two
            // rows carry strictly increasing timestamps even on a coarse clock.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let (status, text) = send(&app, get("/v1/memory/recall?scope=recall-order")).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        let json = parse(&text);
        let data = json["data"].as_array().expect("data is an array");
        assert_eq!(data.len(), 2, "expected both rows back: {json}");
        assert_eq!(data[0]["content"], "first message", "oldest first: {json}");
        assert_eq!(data[1]["content"], "second message", "newest last: {json}");
    }

    #[tokio::test]
    async fn search_finds_inserted_row_and_survives_operators() {
        let (_dir, store) = test_store();
        let app = router(store);
        let body = serde_json::json!({
            "agent": "test-agent",
            "scope": "search-scope",
            "content": "the flux capacitor hums quietly",
        });
        let (status, text) = send(&app, post_json("/v1/memory", &body)).await;
        assert_eq!(status, StatusCode::OK, "insert failed: {text}");

        let (status, text) = send(
            &app,
            get("/v1/memory/search?query=capacitor&scope=search-scope"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        let json = parse(&text);
        let data = json["data"].as_array().expect("data is an array");
        assert_eq!(data.len(), 1, "expected the inserted row: {json}");
        assert_eq!(data[0]["content"], "the flux capacitor hums quietly");

        // Operator-laden input (`AND NOT "unclosed (*` after URL decoding) must
        // not reach FTS5 as syntax — the sanitizer quotes every token, so this
        // is a well-formed (if unmatched) query, never a 5xx.
        let (status, text) = send(
            &app,
            get("/v1/memory/search?query=AND%20NOT%20%22unclosed%20(%2A&scope=search-scope"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "operator query must not error: {text}"
        );
    }

    #[tokio::test]
    async fn recall_with_budget_reports_and_drops_when_tight() {
        let (_dir, store) = test_store();
        let app = router(store);
        for content in ["aaaa", "bbbb", "cccc"] {
            let body = serde_json::json!({
                "agent": "test-agent",
                "scope": "http-budget",
                "content": content,
            });
            let (status, text) = send(&app, post_json("/v1/memory", &body)).await;
            assert_eq!(status, StatusCode::OK, "insert failed: {text}");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Each 4-char memory estimates to 1 token; a 2-token budget keeps
        // the newest two and drops the oldest.
        let (status, text) = send(
            &app,
            get("/v1/memory/recall?scope=http-budget&budget_tokens=2"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        let json = parse(&text);
        let budget = &json["metadata"]["budget"];
        assert_eq!(budget["estimator"], "chars-div-4", "budget: {json}");
        assert_eq!(budget["included"], 2);
        assert_eq!(budget["dropped"], 1);
        let data = json["data"].as_array().expect("data is an array");
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["content"], "bbbb", "oldest dropped: {json}");

        // Without a budget the response is unchanged: no metadata.budget.
        let (status, text) = send(&app, get("/v1/memory/recall?scope=http-budget")).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        assert!(
            parse(&text)["metadata"]["budget"].is_null(),
            "no budget requested, so none reported"
        );
    }

    #[tokio::test]
    async fn context_returns_rules_memories_and_budget() {
        let (_dir, store) = test_store();
        let app = router(store);
        let rule = serde_json::json!({
            "rule_id": "http-context-rule",
            "text": "Context blocks must open with the rules.",
            "scope": "http-context",
        });
        let (status, text) = send(&app, post_json("/v1/rules", &rule)).await;
        assert_eq!(status, StatusCode::OK, "rule add failed: {text}");
        let mem = serde_json::json!({
            "agent": "test-agent",
            "scope": "http-context",
            "content": "a memory for the context block",
        });
        let (status, text) = send(&app, post_json("/v1/memory", &mem)).await;
        assert_eq!(status, StatusCode::OK, "insert failed: {text}");

        let (status, text) = send(&app, get("/v1/context?scope=http-context")).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        let json = parse(&text);
        let data = &json["data"];
        assert_eq!(data["scope"], "http-context");
        assert_eq!(
            data["rules"].as_array().expect("rules is an array").len(),
            1,
            "the rule leads the block: {json}"
        );
        let memories = data["memories"].as_array().expect("memories is an array");
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0]["content"], "a memory for the context block");
        assert!(
            data["budget"].is_object(),
            "data carries the report: {json}"
        );
        assert!(
            json["metadata"]["budget"].is_object(),
            "metadata carries it too: {json}"
        );
    }

    #[tokio::test]
    async fn context_with_unknown_scope_is_200_and_empty() {
        let (_dir, store) = test_store();
        let app = router(store);
        let (status, text) = send(&app, get("/v1/context?scope=never-used")).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        let json = parse(&text);
        assert_eq!(json["data"]["rules"].as_array().map(Vec::len), Some(0));
        assert_eq!(json["data"]["memories"].as_array().map(Vec::len), Some(0));
        assert_eq!(json["data"]["budget"]["included"], 0);
    }

    #[tokio::test]
    async fn recall_without_scope_is_client_error() {
        let (_dir, store) = test_store();
        let app = router(store);
        let (status, text) = send(&app, get("/v1/memory/recall")).await;
        assert!(
            status.is_client_error(),
            "expected 4xx for missing scope, got {status}: {text}"
        );
    }

    #[tokio::test]
    async fn rule_add_rejects_bad_id_then_accepts_valid_body() {
        let (_dir, store) = test_store();
        let app = router(store);

        // Whitespace in the id fails validation before anything is stored.
        let bad = serde_json::json!({
            "rule_id": "has spaces",
            "text": "never stored",
            "scope": "rules-http",
        });
        let (status, text) = send(&app, post_json("/v1/rules", &bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {text}");

        let good = serde_json::json!({
            "rule_id": "http-surface-rule",
            "text": "Keep the HTTP tests green.",
            "scope": "rules-http",
        });
        let (status, text) = send(&app, post_json("/v1/rules", &good)).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        let json = parse(&text);
        assert_eq!(
            json["data"]["rule"]["rule_id"], "http-surface-rule",
            "data.rule missing: {json}"
        );
        assert_eq!(json["data"]["created"], true);
    }

    #[tokio::test]
    async fn delete_unknown_rule_is_404_from_the_handler() {
        let (_dir, store) = test_store();
        let app = router(store);
        let (status, text) = send(&app, delete("/v1/rules/some-unknown-id?scope=rules-404")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "body: {text}");
        // The handler's own message, not axum's bare fallback 404. This locks
        // in the `:rule_id` (axum 0.7 / matchit 0.7) path-param syntax: with
        // the `{rule_id}` form the route never matches, the fallback answers
        // with an empty body, and this assertion fails.
        assert!(
            text.contains("no rule"),
            "expected the handler's 404 body, got: {text}"
        );
    }

    #[tokio::test]
    async fn delete_rule_retires_then_reports_already_retired() {
        let (_dir, store) = test_store();
        let app = router(store);
        let body = serde_json::json!({
            "rule_id": "retire-me",
            "text": "A rule that is about to be withdrawn.",
            "scope": "rules-retire",
        });
        let (status, text) = send(&app, post_json("/v1/rules", &body)).await;
        assert_eq!(status, StatusCode::OK, "rule add failed: {text}");

        let (status, text) = send(&app, delete("/v1/rules/retire-me?scope=rules-retire")).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        assert_eq!(parse(&text)["data"]["outcome"], "retired");

        // Retiring is idempotent: the second DELETE succeeds and says so.
        let (status, text) = send(&app, delete("/v1/rules/retire-me?scope=rules-retire")).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        assert_eq!(parse(&text)["data"]["outcome"], "already-retired");
    }

    #[tokio::test]
    async fn health_reports_ok() {
        let (_dir, store) = test_store();
        let app = router(store);
        let (status, text) = send(&app, get("/v1/health")).await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
        let json = parse(&text);
        assert_eq!(json["status"], "ok", "body: {json}");
        assert!(
            json["timestamp"].as_str().is_some_and(|t| t.ends_with('Z')),
            "timestamp must be ISO 8601 UTC: {json}"
        );
    }
}

// Rust guideline compliant 2026-05-18
