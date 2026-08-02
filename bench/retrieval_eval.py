#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
# SPDX-License-Identifier: GPL-3.0-or-later
"""Black-box retrieval evaluation for the M3 gate.

Drives the real `engram` binary end to end — ingest the corpus with
`remember` (mapping stable corpus keys to the UUIDs engram mints), then run
every held-out query in fts mode and (when a model is given) hybrid mode,
and report recall@5 / recall@10 / MRR per mode and per query kind.

Why a Python driver and not `cargo run --example`: engram is a bin-only
crate, so an example target cannot link its internals; a black-box driver
also measures exactly what a user's shell or agent gets, envelope and all.

Usage:
  python3 bench/retrieval_eval.py --binary target/release/engram \
      [--model /path/to/model2vec/dir] \
      [--corpus bench/corpus.jsonl] [--queries bench/queries.jsonl]

The gate (bench/README.md): hybrid must beat fts on recall@5 by >= 5
points, else vectors ship compiled-off ("measured, declined" is a success
outcome).
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path


def run(binary, db, args, model=None, stdin=None):
    cmd = [binary, "--db", db]
    if model:
        cmd += ["--model-path", model]
    cmd += args
    env = {k: v for k, v in os.environ.items() if k not in (
        "AI_AGENT", "AGENT", "CI", "ENGRAM_DB", "ENGRAM_SCOPE",
        "ENGRAM_AGENT", "ENGRAM_MODEL", "NO_COLOR", "SPACECRAFT_A11Y",
    )}
    # Machine mode without relying on TTY detection.
    env["AI_AGENT"] = "retrieval-eval"
    proc = subprocess.run(cmd, capture_output=True, text=True, input=stdin, env=env)
    if proc.returncode != 0:
        raise RuntimeError(f"{' '.join(args[:2])} failed rc={proc.returncode}: {proc.stderr.strip()[:400]}")
    return json.loads(proc.stdout)


def ingest(binary, db, corpus, model):
    """Store every corpus doc; return key -> uuid."""
    key_to_id = {}
    for doc in corpus:
        env = run(binary, db, [
            "remember", "--agent", doc["agent"], "--scope", doc["scope"],
            "--role", doc["role"], doc["content"],
        ], model=model)
        key_to_id[doc["key"]] = env["data"]["id"]
    return key_to_id


def search_ids(binary, db, query, scope, mode, model, limit=10):
    args = ["search", query, "--limit", str(limit), "--mode", mode]
    if scope:
        args += ["--scope", scope]
    env = run(binary, db, args, model=model)
    return [m["id"] for m in env["data"]]


def evaluate(queries, key_to_id, get_ranked):
    """recall@5 / recall@10 / MRR over the query set, plus per-kind recall@5."""
    r5 = r10 = mrr = 0.0
    by_kind = defaultdict(lambda: [0.0, 0])
    for q in queries:
        relevant = {key_to_id[k] for k in q["relevant_keys"]}
        ranked = get_ranked(q)
        hits5 = len(relevant & set(ranked[:5])) / len(relevant)
        hits10 = len(relevant & set(ranked[:10])) / len(relevant)
        rank = next((i + 1 for i, mid in enumerate(ranked) if mid in relevant), None)
        r5 += hits5
        r10 += hits10
        mrr += (1.0 / rank) if rank else 0.0
        by_kind[q["kind"]][0] += hits5
        by_kind[q["kind"]][1] += 1
    n = len(queries)
    kinds = {k: v[0] / v[1] for k, v in sorted(by_kind.items())}
    return r5 / n, r10 / n, mrr / n, kinds


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/release/engram")
    ap.add_argument("--model", default=None, help="Model2Vec dir; omit to skip hybrid")
    ap.add_argument("--corpus", default="bench/corpus.jsonl")
    ap.add_argument("--queries", default="bench/queries.jsonl")
    args = ap.parse_args()

    corpus = [json.loads(l) for l in Path(args.corpus).read_text().splitlines() if l.strip()]
    queries = [json.loads(l) for l in Path(args.queries).read_text().splitlines() if l.strip()]

    with tempfile.TemporaryDirectory() as tmp:
        db = str(Path(tmp) / "bench.db")
        print(f"ingesting {len(corpus)} docs ...", file=sys.stderr)
        key_to_id = ingest(args.binary, db, corpus, None)

        modes = [("fts", None)]
        if args.model:
            print("indexing vectors ...", file=sys.stderr)
            idx = run(args.binary, db, ["index"], model=args.model)
            print(f"  indexed: {idx['data'].get('indexed', '?')}", file=sys.stderr)
            modes.append(("hybrid", args.model))

        results = {}
        for mode, model in modes:
            print(f"running {len(queries)} queries in {mode} mode ...", file=sys.stderr)
            results[mode] = evaluate(
                queries, key_to_id,
                lambda q, m=mode, mo=model: search_ids(
                    args.binary, db, q["query"], q.get("scope"), m, mo),
            )

    print(f"\n{'mode':<8} {'recall@5':>9} {'recall@10':>10} {'MRR':>7}")
    for mode, (r5, r10, mrr, kinds) in results.items():
        print(f"{mode:<8} {r5:>9.3f} {r10:>10.3f} {mrr:>7.3f}")
        for kind, kr5 in kinds.items():
            print(f"  {kind:<12} recall@5 {kr5:.3f}")

    if "hybrid" in results:
        delta = (results["hybrid"][0] - results["fts"][0]) * 100
        verdict = "PASS" if delta >= 5.0 else "DECLINED"
        print(f"\nGATE: hybrid - fts recall@5 = {delta:+.1f} points -> {verdict}")
        print("(gate: >= +5.0 points; 'measured, declined' is a success outcome)")


if __name__ == "__main__":
    main()
