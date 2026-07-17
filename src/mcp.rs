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
}

#[tool_handler]
impl ServerHandler for EngramMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "Shared verbatim chat memory. Call `remember` after any decision or fact worth \
                 keeping, scoped to a project/task id. Call `recall` at session start for that \
                 scope. Call `search` when unsure whether something was already decided."
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

