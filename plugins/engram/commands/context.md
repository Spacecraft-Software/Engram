---
description: Load this project's engram rules and prior context
argument-hint: "[topic]"
allowed-tools: Bash(engram:*)
---

Load prior context for this project from engram before we continue.

!`engram --db {{DB}} context --budget-tokens 3000 $ARGUMENTS`

Read the result and use it as background:

- **`rules`** are standing policy for this project. They are always included
  in full, even over the token budget, because silently dropping policy is
  worse than exceeding a budget. Treat them as binding for the rest of this
  session.
- **`memories`** are prior conversation and decisions, chronological. When a
  topic argument was given, they were selected by relevance rather than by
  recency alone.
- **`metadata.budget`** reports what was dropped. If `dropped` is large, say
  so — there is more prior context than fits, and a narrower topic argument
  will retrieve better.

Summarize in two or three sentences what this project has already decided,
then continue with whatever the user asked for. Do not restate the rules back
to the user unless they ask; just follow them.
