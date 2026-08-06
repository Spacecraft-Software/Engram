---
description: Capture this session's transcript into engram's memory store
argument-hint: "[scope]"
allowed-tools: Bash(engram:*)
---

Capture this session's transcript into engram, without archiving it.

Scope: `$1`. When empty, engram resolves it from `ENGRAM_SCOPE`, the git
working-tree name, or the current directory name.

1. Show what is available and what would be captured:

   !`engram --db {{DB}} ingest --harness {{HARNESS}} --list`

2. Preview the capture:

   !`engram --db {{DB}} ingest --harness {{HARNESS}} --dry-run`

3. If the user is happy with the preview, capture for real:

   !`engram --db {{DB}} ingest --harness {{HARNESS}}`

Report the number inserted and skipped. `skipped_existing` counting most of
the turns means this session was already captured — that is the expected
result of running this twice, not an error.

Note for the user if asked: tool payloads and model thinking are excluded by
default. `--include-tools` adds one summary line per tool call; it never
stores the payload itself.
