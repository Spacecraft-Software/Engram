// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! HTTP API. Same Store, same data shapes as the CLI and MCP surfaces —
//! for tools that don't speak MCP (curl, Nushell `http`, other services).
//! Local-only by default (PFA: bind 127.0.0.1, no auth, no telemetry).
//! Add a Bearer token check before exposing this beyond localhost.

use crate::output::envelope::Response;
use crate::store::Store;
use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};

type SharedStore = Arc<Mutex<Store>>;

pub fn router(store: SharedStore) -> Router {
    Router::new()
        .route("/v1/memory", post(remember))
        .route("/v1/memory/recall", get(recall))
        .route("/v1/memory/search", get(search))
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

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "timestamp": crate::time::now_iso8601() }))
}
