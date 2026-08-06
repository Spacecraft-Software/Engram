---
description: Capture this session into engram and archive it as Texinfo
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
   traffic, so a large `tool_result` count is normal, not a problem. If
   `unknown_record` is non-zero, say so — it means this harness's transcript
   format has changed and the reader may be missing messages.

3. If step 1 reports turns to insert, run it for real:

   !`engram --db {{DB}} ingest --harness {{HARNESS}}`

4. Archive the scope:

   !`engram --db {{DB}} save-chat --scope $1`

5. Report the archive path, the message count, and the write outcome. An
   outcome of `unchanged` is a success, not a no-op failure: it means the
   archive already matched the scope exactly.

If step 1 or 3 fails with `INVALID_ARGUMENT`, read the `hint` — engram
explains there whether the harness has no transcript reader, and what to do
instead.
