---
description: Save this conversation: capture the transcript into engram's memory store, then archive it as a Texinfo document
argument-hint: "[scope]"
allowed-tools: Bash(engram:*)
---

Capture this session into engram, then archive it.

Scope: `$1`. When empty, engram resolves it itself — `ENGRAM_SCOPE`, then the
git working-tree name, then the current directory name — so passing nothing is
usually right.

Run these in order and report what happened:

1. Preview the capture. This writes nothing:

   !`engram --db {{DB}} ingest --harness {{HARNESS}} --dry-run`

2. Read the `filtered` counts in that output. Most of a transcript is tool
   traffic, so a large `tool_result` count is normal, not a problem.

   Three counters mean a record could not be read, and they are not
   interchangeable. Report whichever is non-zero, and say which it was:

   - `unknown_record` — a record `type` engram does not recognize. This is the
     one that means the harness's transcript format has moved and the reader
     may be missing messages. Worth acting on.
   - `torn_line` — a line that is not valid JSON, from a write interrupted
     mid-flight. Nothing in engram is wrong. Often *transient*: reading a
     transcript the harness is still appending to catches partial lines that
     are complete moments later, so re-running usually shows fewer. Do not
     report this as a format change.
   - `missing_uuid` — a conversation record with no `uuid`, which cannot be
     given a stable id. This is the one where a real turn was dropped.

3. If step 1 reports turns to insert, run it for real:

   !`engram --db {{DB}} ingest --harness {{HARNESS}}`

4. Archive the scope:

   !`engram --db {{DB}} save-chat --scope "$1"`

5. Report the archive path, the message count, and the write outcome. An
   outcome of `unchanged` is a success, not a no-op failure: it means the
   archive already matched the scope exactly.

If step 1 or 3 fails with `INVALID_ARGUMENT`, read the `hint` — engram
explains there whether the harness has no transcript reader, and what to do
instead.
