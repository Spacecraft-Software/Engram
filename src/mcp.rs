// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! MCP surface. Three tools, dispatching to the exact same `Store` methods
//! the CLI and HTTP API use — one source of truth, three transports.
//!
//! NOTE: written against rmcp 0.16's `#[tool_router]`/`#[tool_handler]`
//! macros per the official examples (modelcontextprotocol/rust-sdk). This
//! crate could not be compiled in the environment that generated it —
//! run `cargo build` and check `cargo doc -p rmcp --open` against the
//! pinned version if the macro shape has drifted.

use crate::store::Store;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, ReadBuf};

#[derive(Clone)]
pub struct EngramMcp {
    store: Arc<Mutex<Store>>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberArgs {
    /// Which agent is writing this memory (e.g. "claude-code", "codex", "kimi").
    pub agent: String,
    /// Grouping key — a project, task id, or pipeline run id.
    pub scope: String,
    /// "user" | "assistant" | "system" | "note"
    #[serde(default = "default_role")]
    pub role: String,
    pub content: String,
}
fn default_role() -> String { "note".to_string() }

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallArgs {
    pub scope: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    pub scope: Option<String>,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
}
fn default_limit() -> u32 { 50 }
fn default_search_limit() -> u32 { 20 }

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RuleAddArgs {
    /// Stable kebab-case identifier, unique within the scope. Re-using an
    /// existing id revises that rule in place instead of adding a second one.
    pub rule_id: String,
    /// The rule itself, as plain prose.
    pub text: String,
    /// Scope to file the rule under. Omit to use the scope resolved from the
    /// server process's working directory — pass it explicitly whenever this
    /// server is shared across projects.
    pub scope: Option<String>,
    /// Which agent/model is recording the rule.
    #[serde(default = "default_agent")]
    pub agent: String,
}
fn default_agent() -> String { "mcp-client".to_string() }

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RuleListArgs {
    /// Scope to read. Omit to use the server process's resolved scope.
    pub scope: Option<String>,
    /// Also return withdrawn rules, each flagged `"retired": true`.
    #[serde(default)]
    pub include_retired: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RuleRetireArgs {
    /// Id of the rule to withdraw.
    pub rule_id: String,
    /// Scope to read. Omit to use the server process's resolved scope.
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RuleSyncArgs {
    /// Scope to render. Omit to use the server process's resolved scope.
    pub scope: Option<String>,
    /// Report what would be written without touching any file.
    #[serde(default)]
    pub dry_run: bool,
}

#[tool_router]
impl EngramMcp {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store, tool_router: Self::tool_router() }
    }

    #[tool(description = "Store a verbatim chat message in shared memory, scoped to a project/task/run id.")]
    async fn remember(&self, Parameters(args): Parameters<RememberArgs>) -> Result<CallToolResult, McpError> {
        let store = self.store.lock().map_err(lock_err)?;
        let mem = store
            .remember(&args.agent, &args.scope, &args.role, &args.content)
            .map_err(store_err)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string(&mem).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Read back the last N memories for a scope, in chronological order.")]
    async fn recall(&self, Parameters(args): Parameters<RecallArgs>) -> Result<CallToolResult, McpError> {
        let store = self.store.lock().map_err(lock_err)?;
        let mems = store.recall(&args.scope, args.limit).map_err(store_err)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string(&mems).unwrap_or_default(),
        )]))
    }

    #[tool(description = "Full-text search across stored memories, optionally restricted to one scope.")]
    async fn search(&self, Parameters(args): Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        let store = self.store.lock().map_err(lock_err)?;
        let mems = store
            .search(&args.query, args.scope.as_deref(), args.limit)
            .map_err(store_err)?;
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string(&mems).unwrap_or_default(),
        )]))
    }

    #[tool(
        description = "Record a durable project rule, or revise the existing rule with the same rule_id. \
                       Use for policy that must keep applying in later sessions — coding standards, \
                       packaging gates, review requirements — not for one-off facts (use `remember`). \
                       Storing a rule does not surface it to anyone: call `rule_sync` afterwards."
    )]
    async fn rule_add(&self, Parameters(args): Parameters<RuleAddArgs>) -> Result<CallToolResult, McpError> {
        crate::rules::validate_rule_id(&args.rule_id).map_err(|e| McpError::invalid_params(e, None))?;
        crate::rules::validate_rule_text(&args.text).map_err(|e| McpError::invalid_params(e, None))?;
        let resolved = crate::rules::resolve_scope(args.scope.as_deref()).map_err(scope_err)?;

        let store = self.store.lock().map_err(lock_err)?;
        let upsert = store
            .rule_add(&args.agent, &resolved.name, &args.rule_id, args.text.trim())
            .map_err(store_err)?;

        let payload = serde_json::json!({
            "rule": upsert.rule,
            "created": upsert.created,
            "scope": resolved.name,
            "scope_origin": resolved.origin,
            "next_step": "call rule_sync to render this rule into AGENTS.md and CLAUDE.md; \
                          until then no agent will read it",
        });
        Ok(CallToolResult::success(vec![Content::text(payload.to_string())]))
    }

    #[tool(
        description = "List the durable rules in effect for a scope, ordered by rule_id. Call this \
                       before asserting that something is or is not project policy."
    )]
    async fn rule_list(&self, Parameters(args): Parameters<RuleListArgs>) -> Result<CallToolResult, McpError> {
        let resolved = crate::rules::resolve_scope(args.scope.as_deref()).map_err(scope_err)?;
        let store = self.store.lock().map_err(lock_err)?;
        let rules = if args.include_retired {
            store.rules_including_retired(&resolved.name).map_err(store_err)?
        } else {
            store.rules(&resolved.name).map_err(store_err)?
        };

        let payload = serde_json::json!({
            "scope": resolved.name,
            "scope_origin": resolved.origin,
            "count": rules.len(),
            "rules": rules,
        });
        Ok(CallToolResult::success(vec![Content::text(payload.to_string())]))
    }

    #[tool(
        description = "Withdraw a rule so it stops being binding. Tombstones rather than deletes: \
                       the rule leaves `rule_list` and synced files but stays in the database and \
                       remains findable via `search`. Re-running `rule_add` with the same rule_id \
                       reinstates it. Call `rule_sync` afterwards, or the markdown keeps asserting \
                       the retired rule."
    )]
    async fn rule_retire(&self, Parameters(args): Parameters<RuleRetireArgs>) -> Result<CallToolResult, McpError> {
        let resolved = crate::rules::resolve_scope(args.scope.as_deref()).map_err(scope_err)?;
        let store = self.store.lock().map_err(lock_err)?;
        let retire = store.rule_retire(&resolved.name, &args.rule_id).map_err(store_err)?;

        if retire.outcome == crate::store::RetireOutcome::NotFound {
            return Err(McpError::invalid_params(
                format!("no rule '{}' in scope '{}'", args.rule_id, resolved.name),
                None,
            ));
        }

        let payload = serde_json::json!({
            "rule_id": retire.rule_id,
            "scope": retire.scope,
            "outcome": retire.outcome,
            "rule": retire.rule,
            "scope_origin": resolved.origin,
            "next_step": "call rule_sync to drop this rule from AGENTS.md and CLAUDE.md; \
                          until then the synced files still assert it",
        });
        Ok(CallToolResult::success(vec![Content::text(payload.to_string())]))
    }

    #[tool(
        description = "Render a scope's rules into AGENTS.md and CLAUDE.md at the project root, \
                       rewriting only the region between the engram sentinels and leaving the rest \
                       of each file untouched. Idempotent: re-running with unchanged rules writes \
                       nothing. This is the step that actually puts rules in front of a model."
    )]
    async fn rule_sync(&self, Parameters(args): Parameters<RuleSyncArgs>) -> Result<CallToolResult, McpError> {
        let resolved = crate::rules::resolve_scope(args.scope.as_deref()).map_err(scope_err)?;
        let rules = {
            let store = self.store.lock().map_err(lock_err)?;
            store.rules(&resolved.name).map_err(store_err)?
        };

        let block = crate::rules::render_block(&resolved.name, &rules);
        let mut written = Vec::new();
        for path in crate::rules::target_paths(&resolved.root, &[]) {
            let file = crate::rules::sync_file(&path, &block, args.dry_run).map_err(|e| {
                McpError::internal_error(format!("failed to sync {}: {e}", path.display()), None)
            })?;
            written.push(file);
        }

        let payload = serde_json::json!({
            "scope": resolved.name,
            "scope_origin": resolved.origin,
            "root": resolved.root.to_string_lossy(),
            "rule_count": rules.len(),
            "dry_run": args.dry_run,
            "files": written,
        });
        Ok(CallToolResult::success(vec![Content::text(payload.to_string())]))
    }
}

#[tool_handler]
impl ServerHandler for EngramMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "Shared verbatim chat memory plus durable project rules.\n\n\
                 Memory: call `remember` after any decision or fact worth keeping, scoped to a \
                 project/task id. Call `recall` at session start for that scope. Call `search` \
                 when unsure whether something was already decided.\n\n\
                 Rules: call `rule_list` at session start to load the policy governing this \
                 project. Call `rule_add` when the user states a standing requirement — \
                 something that must keep applying in future sessions, not a one-off fact — then \
                 `rule_sync` to render it into AGENTS.md and CLAUDE.md. A stored rule that is \
                 never synced is read by nobody. Call `rule_retire` when a rule no longer \
                 applies, then `rule_sync` again."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}

fn lock_err<T>(_: std::sync::PoisonError<T>) -> McpError {
    McpError::internal_error("engram store lock poisoned", None)
}
fn store_err(e: rusqlite::Error) -> McpError {
    McpError::internal_error(format!("engram storage error: {e}"), None)
}
fn scope_err(e: std::io::Error) -> McpError {
    McpError::internal_error(format!("could not resolve scope: {e}"), None)
}

/// Some MCP hosts (observed: Antigravity's plugin loader) probe a server with a
/// proprietary request — e.g. `server/discover` — before the spec-mandated
/// `initialize`. rmcp 0.16's `serve()` treats any non-`initialize` first frame as
/// fatal and tears the connection down (`ExpectedInitializeRequest`), so a strict
/// probe like that permanently breaks the handshake. Drain and reject any such
/// leading frames here — same tolerance crates-mcp's hand-rolled dispatch loop
/// already gives these probes — before handing the stream to rmcp.
const MAX_LEADING_PROBES: u32 = 32;

pub async fn run_stdio(store: Arc<Mutex<Store>>) -> anyhow::Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();
    let mut probes = 0u32;

    let prefix = loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF before a real initialize ever arrived; nothing to hand off.
            return Ok(());
        }

        let is_initialize = serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_string))
            .map(|method| method == "initialize");

        match is_initialize {
            Some(true) | None => break line.clone().into_bytes(),
            Some(false) => {
                probes += 1;
                reject_non_initialize(&mut stdout, &line).await?;
                if probes >= MAX_LEADING_PROBES {
                    // Give up rejecting forever; hand off whatever comes next and
                    // let rmcp report its own error if it's still not initialize.
                    line.clear();
                    reader.read_line(&mut line).await?;
                    break line.clone().into_bytes();
                }
            }
        }
    };

    let service = EngramMcp::new(store).serve((PrefixedReader::new(prefix, reader), stdout)).await?;
    service.waiting().await?;
    Ok(())
}

/// Reply to a leading non-`initialize` frame with a standard JSON-RPC
/// "method not found" error, echoing the request id when present, and drop the
/// frame — the client is expected to send `initialize` next.
async fn reject_non_initialize(
    stdout: &mut tokio::io::Stdout,
    raw_line: &str,
) -> anyhow::Result<()> {
    let id = serde_json::from_str::<serde_json::Value>(raw_line)
        .ok()
        .and_then(|v| v.get("id").cloned());

    // No id means it's a notification; per JSON-RPC 2.0, notifications get no reply.
    let Some(id) = id else { return Ok(()) };

    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32601, "message": "expected initialize request" }
    });
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    stdout.write_all(&bytes).await?;
    stdout.flush().await?;
    Ok(())
}

/// An `AsyncRead` that first replays a captured prefix (the already-consumed
/// `initialize` line), then delegates to the underlying buffered reader.
struct PrefixedReader<R> {
    prefix: Cursor<Vec<u8>>,
    inner: R,
}

impl<R> PrefixedReader<R> {
    fn new(prefix: Vec<u8>, inner: R) -> Self {
        Self { prefix: Cursor::new(prefix), inner }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for PrefixedReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.prefix.position() < self.prefix.get_ref().len() as u64 {
            let before = buf.filled().len();
            std::io::Read::read(&mut self.prefix, buf.initialize_unfilled())
                .map(|n| buf.advance(n))
                .ok();
            if buf.filled().len() > before {
                return Poll::Ready(Ok(()));
            }
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

// Rust guideline compliant 2026-05-18

