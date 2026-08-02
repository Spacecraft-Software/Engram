#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
# SPDX-License-Identifier: GPL-3.0-or-later
"""Validate the engram retrieval benchmark dataset.

Checks, in order:
  1. every line of corpus.jsonl and queries.jsonl parses as a JSON object;
  2. required fields are present, correctly typed, and non-empty;
  3. corpus keys are unique;
  4. every query's relevant_keys resolve to corpus keys;
  5. query kinds belong to the documented taxonomy;
  6. corpus content lengths stay within 20..800 chars;
  7. query scopes, corpus scopes/agents/roles use the documented vocabularies.

Exit code 0 with per-kind counts on success; non-zero with one line per
problem on failure. No third-party dependencies; python3 only.
"""

from __future__ import annotations

import json
import sys
from collections import Counter
from pathlib import Path

BENCH_DIR = Path(__file__).resolve().parent

KINDS = ("exact-term", "paraphrase", "synonym", "conceptual", "multi-fact")
AGENTS = {"claude-code", "codex", "kimi", "gemini-cli", "human"}
ROLES = {"note", "assistant", "user", "system"}
CONTENT_MIN, CONTENT_MAX = 20, 800

CORPUS_FIELDS = {"key": str, "agent": str, "scope": str, "role": str,
                 "content": str}
QUERY_FIELDS = {"query": str, "scope": (str, type(None)),
                "relevant_keys": list, "kind": str}


def load_jsonl(path: Path, errors: list[str]) -> list[tuple[int, dict]]:
    rows: list[tuple[int, dict]] = []
    with path.open(encoding="utf-8") as handle:
        for lineno, line in enumerate(handle, start=1):
            if not line.strip():
                errors.append(f"{path.name}:{lineno}: blank line")
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError as exc:
                errors.append(f"{path.name}:{lineno}: parse error: {exc}")
                continue
            if not isinstance(obj, dict):
                errors.append(f"{path.name}:{lineno}: not a JSON object")
                continue
            rows.append((lineno, obj))
    return rows


def check_fields(name: str, lineno: int, obj: dict, spec: dict,
                 errors: list[str]) -> bool:
    ok = True
    for field, types in spec.items():
        if field not in obj:
            errors.append(f"{name}:{lineno}: missing field {field!r}")
            ok = False
        elif not isinstance(obj[field], types):
            errors.append(
                f"{name}:{lineno}: field {field!r} has wrong type "
                f"{type(obj[field]).__name__}")
            ok = False
    for field in obj:
        if field not in spec:
            errors.append(f"{name}:{lineno}: unexpected field {field!r}")
            ok = False
    return ok


def main() -> int:
    errors: list[str] = []
    corpus = load_jsonl(BENCH_DIR / "corpus.jsonl", errors)
    queries = load_jsonl(BENCH_DIR / "queries.jsonl", errors)

    keys: dict[str, int] = {}
    scopes: set[str] = set()
    for lineno, doc in corpus:
        if not check_fields("corpus.jsonl", lineno, doc, CORPUS_FIELDS,
                            errors):
            continue
        key = doc["key"]
        if key in keys:
            errors.append(
                f"corpus.jsonl:{lineno}: duplicate key {key!r} "
                f"(first seen line {keys[key]})")
        else:
            keys[key] = lineno
        if doc["agent"] not in AGENTS:
            errors.append(
                f"corpus.jsonl:{lineno}: unknown agent {doc['agent']!r}")
        if doc["role"] not in ROLES:
            errors.append(
                f"corpus.jsonl:{lineno}: unknown role {doc['role']!r}")
        if not doc["scope"]:
            errors.append(f"corpus.jsonl:{lineno}: empty scope")
        scopes.add(doc["scope"])
        length = len(doc["content"])
        if not CONTENT_MIN <= length <= CONTENT_MAX:
            errors.append(
                f"corpus.jsonl:{lineno}: content length {length} outside "
                f"{CONTENT_MIN}..{CONTENT_MAX}")

    kind_counts: Counter[str] = Counter()
    for lineno, query in queries:
        if not check_fields("queries.jsonl", lineno, query, QUERY_FIELDS,
                            errors):
            continue
        if query["kind"] not in KINDS:
            errors.append(
                f"queries.jsonl:{lineno}: unknown kind {query['kind']!r}")
        else:
            kind_counts[query["kind"]] += 1
        if not query["query"].strip():
            errors.append(f"queries.jsonl:{lineno}: empty query text")
        if query["scope"] is not None and query["scope"] not in scopes:
            errors.append(
                f"queries.jsonl:{lineno}: scope {query['scope']!r} not "
                f"present in corpus")
        rel = query["relevant_keys"]
        if not rel:
            errors.append(f"queries.jsonl:{lineno}: empty relevant_keys")
        if len(rel) != len(set(rel)):
            errors.append(
                f"queries.jsonl:{lineno}: duplicate entries in "
                f"relevant_keys")
        for key in rel:
            if not isinstance(key, str):
                errors.append(
                    f"queries.jsonl:{lineno}: non-string relevant key "
                    f"{key!r}")
            elif key not in keys:
                errors.append(
                    f"queries.jsonl:{lineno}: relevant key {key!r} not in "
                    f"corpus")

    if errors:
        for line in errors:
            print(line, file=sys.stderr)
        print(f"FAIL: {len(errors)} problem(s)", file=sys.stderr)
        return 1

    total = sum(kind_counts.values())
    print(f"corpus.jsonl: {len(corpus)} documents, "
          f"{len(scopes)} scopes, all keys unique")
    print(f"queries.jsonl: {total} queries, all relevant_keys resolve")
    for kind in KINDS:
        count = kind_counts.get(kind, 0)
        share = 100.0 * count / total if total else 0.0
        print(f"  {kind:<11} {count:>3}  ({share:.1f}%)")
    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
