<!--
SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Transcript fixtures

These files are **synthetic**. They are hand-written to exercise every record
type, content shape, and flag the readers in `src/transcript/` handle.

They are not copies of real sessions, and a real session must never be
committed here. A transcript records everything a user pasted into it —
`.env` contents, diffs, API keys, customer data. That is the same reason
`engram ingest` redacts before storing and excludes tool payloads by default;
checking a real one into a public repository would defeat the point rather
loudly.

When a harness changes its format, extend the fixture with the new shape
rather than replacing it: the old shapes still exist in transcripts already
on disk, and the reader has to keep handling them.

## `claude-code/session-basic.jsonl`

One synthetic Claude Code session covering:

| Line | Exercises |
|------|-----------|
| `mode`, `permission-mode`, `file-history-snapshot`, `ai-title` | recognized non-message records |
| `something-new-in-a-future-release` | an **unknown** record type, which must be counted rather than silently skipped |
| `user` with `isMeta: true` | harness bookkeeping injected as a turn |
| `user` with `<local-command-stdout>` | a synthetic slash-command echo |
| `user` with a bare-string `content` | the shape that is *not* a block array |
| `user` with a `tool_result` block array | the common shape, dropped by default |
| `user` carrying an ANSI escape and a `<system-reminder>` | text normalization |
| `user` carrying a credential | redaction before storage |
| `assistant` with `thinking` + `text` blocks | block flattening with thinking dropped |
| `assistant` with a `tool_use` block | tool summarization, payload never inlined |
| `user` with `isSidechain: true` | subagent chatter, excluded by default |

## `codex/rollout-basic.jsonl`

One synthetic Codex rollout. `{{CWD}}` is substituted by the test with the
temporary project directory, because Codex matches a session to a project by
the `cwd` recorded in `session_meta` rather than by a mangled directory name.

| Record | Exercises |
|--------|-----------|
| `session_meta` | the `cwd` and `session_id` that listing depends on |
| `turn_context`, `task_started`, `task_complete`, `token_count` | recognized non-conversation records |
| `something_new_in_a_future_release` | an **unknown** event type, counted rather than skipped |
| `event_msg` / `user_message`, `agent_message` | the primary channel, flat strings |
| `agent_message` with `commentary` **and** `final_answer` phases | both are real prose; neither summarizes the other |
| `event_msg` / `agent_reasoning` | thinking, excluded by default |
| `response_item` / `message` role `developer` | instructions to the model, not conversation |
| `response_item` / `message` role `user` holding `<environment_context>` | the injected block that makes the raw channel *noisier* than the display channel — the reason `event_msg` is primary |
| `response_item` duplicating an `event_msg` turn | the deduplication that keeps the display channel authoritative |
| `function_call` / `function_call_output` | tool traffic, counted, payload never stored |
| a credential in a `user_message` | redaction before storage |
