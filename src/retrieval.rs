// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Token budgeting and retrieval assembly — pure functions, no database
//! handle.
//!
//! Everything here operates on values the caller already fetched from
//! [`crate::store::Store`]: estimate a token cost, fuse ranked candidate
//! lists, and greedily pack candidates into a budget. Keeping this module
//! side-effect-free is what makes every step unit-testable and identical
//! across the CLI, MCP, and HTTP surfaces.

use crate::store::Memory;
use serde::Serialize;
use std::collections::BTreeMap;

/// Reciprocal-rank-fusion constant.
///
/// `k = 60` per Cormack, Clarke & Büttcher (2009), "Reciprocal Rank Fusion
/// outperforms Condorcet and individual Rank Learning Methods" — the value
/// their evaluation found near-optimal, since adopted as the de-facto
/// default. Raising it flattens the difference between ranks; lowering it
/// sharpens the reward for rank 0.
pub const RRF_K: f64 = 60.0;

/// Name of the token estimator, reported in every [`BudgetReport`].
///
/// Callers seeing this string know exactly which heuristic produced
/// `estimated_tokens` and can re-derive or replace it downstream.
pub const ESTIMATOR: &str = "chars-div-4";

/// Estimates the token cost of `text` as `ceil(chars / 4)`, minimum 1.
///
/// Multi-model pipelines have no single correct tokenizer — every model in
/// the pipeline tokenizes differently — so engram uses a deliberately crude,
/// model-agnostic heuristic (~4 characters per token, counted in Unicode
/// characters, not bytes) and reports it as estimator [`ESTIMATOR`]. Even an
/// empty string costs 1: any included item occupies at least one token's
/// worth of context once serialized.
pub fn estimate_tokens(text: &str) -> u32 {
    let chars = text.chars().count();
    // Magic 4: the industry rule-of-thumb of ~4 chars/token for English-like
    // text. Changing it changes every budget decision but nothing else.
    let estimate = chars.div_ceil(4).max(1);
    u32::try_from(estimate).unwrap_or(u32::MAX)
}

/// Fuses ranked candidate lists with reciprocal rank fusion.
///
/// `score(id) = Σ over channels of 1 / (k + rank)`, where `rank` is the
/// id's 0-based position in that channel; ids absent from a channel
/// contribute nothing there. The output is sorted by score descending, then
/// id ascending — a total order, so the result is deterministic for a given
/// input regardless of hash seeds or call count.
pub fn rrf_fuse(channels: &[Vec<String>], k: f64) -> Vec<(String, f64)> {
    let mut scores: BTreeMap<String, f64> = BTreeMap::new();
    for channel in channels {
        for (rank, id) in channel.iter().enumerate() {
            // Ranks are tiny (bounded by fetch limits); the usize→f64 cast
            // is lossless in practice.
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (k + rank as f64);
        }
    }
    let mut fused: Vec<(String, f64)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    fused
}

/// How a budget-packing pass went, attached to responses as
/// `metadata.budget` (and inside `context` data) so callers can see what was
/// kept, what was cut, and by which yardstick.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetReport {
    /// The budget the caller asked for, in estimated tokens.
    pub requested_tokens: u32,
    /// Which heuristic produced the estimates — always [`ESTIMATOR`].
    pub estimator: &'static str,
    /// Estimated tokens summed over the *included* items only.
    pub estimated_tokens: u32,
    /// Count of included items.
    pub included: usize,
    /// Count of dropped candidates.
    pub dropped: usize,
    /// Ids of dropped candidates, in the order they were dropped.
    pub dropped_ids: Vec<String>,
    /// Candidate count fed in per channel. `BTreeMap` for deterministic
    /// serialization order.
    pub channels: BTreeMap<&'static str, usize>,
}

/// Outcome of a [`greedy_pack`]: which priority positions fit, which ids
/// did not, and the token total of what fit.
#[derive(Debug)]
pub(crate) struct PackOutcome {
    /// Indices into the priority-ordered input of the included items, in
    /// priority order.
    pub included: Vec<usize>,
    /// Ids of the dropped items, in the order they were dropped.
    pub dropped_ids: Vec<String>,
    /// Sum of estimated tokens over the included items. Never exceeds the
    /// budget, so it cannot overflow `u32`.
    pub tokens: u32,
}

/// Greedily packs priority-ordered `(id, token_cost)` candidates into
/// `budget_tokens`.
///
/// An item is included iff its cost fits in the *remaining* budget;
/// otherwise it is dropped and later (cheaper) candidates are still tried —
/// drop-and-continue, not first-failure-stops.
pub(crate) fn greedy_pack(costed: &[(&str, u32)], budget_tokens: u32) -> PackOutcome {
    let mut remaining = budget_tokens;
    let mut included = Vec::new();
    let mut dropped_ids = Vec::new();
    let mut tokens = 0u32;
    for (index, (id, cost)) in costed.iter().enumerate() {
        if *cost <= remaining {
            remaining -= cost;
            tokens += cost;
            included.push(index);
        } else {
            dropped_ids.push((*id).to_string());
        }
    }
    PackOutcome {
        included,
        dropped_ids,
        tokens,
    }
}

/// Packs recall output into a token budget, newest first.
///
/// `mems` is chronological (oldest first), exactly as [`crate::store::Store::recall`]
/// returns it. Priority is newest-first — when the budget bites, it is the
/// *oldest* memories that go. The included items are returned re-sorted back
/// to chronological order for prompt reading.
pub fn budget_recall(mems: Vec<Memory>, budget_tokens: u32) -> (Vec<Memory>, BudgetReport) {
    let input_len = mems.len();
    let pack = {
        let costed: Vec<(&str, u32)> = mems
            .iter()
            .rev() // newest-first priority over chronological input
            .map(|m| (m.id.as_str(), estimate_tokens(&m.content)))
            .collect();
        greedy_pack(&costed, budget_tokens)
    };

    // Map priority positions (newest-first) back onto input positions, then
    // keep the included items in their original chronological order.
    let mut keep = vec![false; input_len];
    for priority_index in &pack.included {
        keep[input_len - 1 - priority_index] = true;
    }
    let included: Vec<Memory> = mems
        .into_iter()
        .zip(keep)
        .filter_map(|(mem, kept)| kept.then_some(mem))
        .collect();

    let report = BudgetReport {
        requested_tokens: budget_tokens,
        estimator: ESTIMATOR,
        estimated_tokens: pack.tokens,
        included: included.len(),
        dropped: pack.dropped_ids.len(),
        dropped_ids: pack.dropped_ids,
        channels: BTreeMap::from([("recency", input_len)]),
    };
    (included, report)
}

/// Packs search output into a token budget, best rank first.
///
/// `mems` is in rank order, exactly as [`crate::store::Store::search`]
/// returns it; rank is both the priority and the output order — relevance
/// ordering is the point of a search result, so it is preserved.
pub fn budget_search(mems: Vec<Memory>, budget_tokens: u32) -> (Vec<Memory>, BudgetReport) {
    let input_len = mems.len();
    let pack = {
        let costed: Vec<(&str, u32)> = mems
            .iter()
            .map(|m| (m.id.as_str(), estimate_tokens(&m.content)))
            .collect();
        greedy_pack(&costed, budget_tokens)
    };

    let mut keep = vec![false; input_len];
    for index in &pack.included {
        keep[*index] = true;
    }
    let included: Vec<Memory> = mems
        .into_iter()
        .zip(keep)
        .filter_map(|(mem, kept)| kept.then_some(mem))
        .collect();

    let report = BudgetReport {
        requested_tokens: budget_tokens,
        estimator: ESTIMATOR,
        estimated_tokens: pack.tokens,
        included: included.len(),
        dropped: pack.dropped_ids.len(),
        dropped_ids: pack.dropped_ids,
        channels: BTreeMap::from([("fts", input_len)]),
    };
    (included, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(id: &str, created_at: &str, content: &str) -> Memory {
        Memory {
            id: id.to_string(),
            agent: "test-agent".to_string(),
            scope: "retrieval-tests".to_string(),
            role: "note".to_string(),
            content: content.to_string(),
            created_at: created_at.to_string(),
            rule_id: None,
            updated_at: None,
            valid_from: None,
            valid_to: None,
            superseded_by: None,
        }
    }

    fn contents(mems: &[Memory]) -> Vec<&str> {
        mems.iter().map(|m| m.content.as_str()).collect()
    }

    #[test]
    fn estimate_tokens_is_ceil_chars_div_4_with_floor_1() {
        assert_eq!(estimate_tokens(""), 1, "empty still costs one token");
        assert_eq!(estimate_tokens("abcd"), 1, "4 chars fit one token");
        assert_eq!(estimate_tokens("abcde"), 2, "5 chars round up");
        // Multibyte content is counted in characters, not bytes: five
        // rocket emoji are 20 UTF-8 bytes but 5 chars.
        assert_eq!(estimate_tokens("🚀🚀🚀🚀🚀"), 2);
        assert_eq!(estimate_tokens("日本語"), 1);
    }

    #[test]
    fn rrf_fuse_matches_hand_computed_scores() {
        let channel_a = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let channel_b = vec!["a".to_string(), "c".to_string()];
        let fused = rrf_fuse(&[channel_a, channel_b], RRF_K);

        // a: rank 0 in both channels; c: rank 2 and rank 1; b: rank 1 in one.
        let expected = vec![
            ("a".to_string(), 1.0 / 60.0 + 1.0 / 60.0),
            ("c".to_string(), 1.0 / 62.0 + 1.0 / 61.0),
            ("b".to_string(), 1.0 / 61.0),
        ];
        assert_eq!(fused, expected);
    }

    #[test]
    fn rrf_fuse_breaks_score_ties_by_id_ascending() {
        // Both ids sit at rank 0 of their own channel: identical scores.
        let fused = rrf_fuse(
            &[vec!["zeta".to_string()], vec!["alpha".to_string()]],
            RRF_K,
        );
        assert_eq!(fused[0].1, fused[1].1, "scores tie by construction");
        assert_eq!(fused[0].0, "alpha", "ties resolve by id ascending");
        assert_eq!(fused[1].0, "zeta");
    }

    #[test]
    fn rrf_fuse_ranks_an_id_in_both_channels_above_an_id_in_one() {
        // "both" is rank 1 in channel A and rank 0 in channel B; "solo" is
        // rank 0 in channel A only. 1/61 + 1/60 > 1/60.
        let channel_a = vec!["solo".to_string(), "both".to_string()];
        let channel_b = vec!["both".to_string()];
        let fused = rrf_fuse(&[channel_a, channel_b], RRF_K);
        assert_eq!(fused[0].0, "both");
        assert_eq!(fused[1].0, "solo");
    }

    #[test]
    fn rrf_fuse_is_deterministic_across_repeated_calls() {
        let channels = [
            vec!["m3".to_string(), "m1".to_string(), "m2".to_string()],
            vec!["m2".to_string(), "m3".to_string()],
        ];
        assert_eq!(rrf_fuse(&channels, RRF_K), rrf_fuse(&channels, RRF_K));
    }

    #[test]
    fn budget_recall_drops_oldest_first_and_keeps_output_chronological() {
        let mems = vec![
            mem("m1", "2026-01-01T00:00:01Z", "aaaa"), // 1 token, oldest
            mem("m2", "2026-01-01T00:00:02Z", "bbbb"), // 1 token
            mem("m3", "2026-01-01T00:00:03Z", "cccc"), // 1 token, newest
        ];
        let (kept, report) = budget_recall(mems, 2);
        assert_eq!(
            contents(&kept),
            ["bbbb", "cccc"],
            "oldest dropped, survivors chronological"
        );
        assert_eq!(report.dropped_ids, ["m1"]);
        assert_eq!(report.included, 2);
        assert_eq!(report.dropped, 1);
        assert_eq!(report.channels[&"recency"], 3);
    }

    #[test]
    fn budget_recall_includes_an_exact_fit() {
        let mems = vec![
            mem("m1", "2026-01-01T00:00:01Z", "aaaa"),
            mem("m2", "2026-01-01T00:00:02Z", "bbbb"),
            mem("m3", "2026-01-01T00:00:03Z", "cccc"),
        ];
        let (kept, report) = budget_recall(mems, 3);
        assert_eq!(kept.len(), 3, "an exactly-sufficient budget keeps all");
        assert_eq!(report.dropped, 0);
        assert_eq!(report.estimated_tokens, 3);
    }

    #[test]
    fn budget_recall_skips_an_oversized_item_but_keeps_trying() {
        let mems = vec![
            mem("small-old", "2026-01-01T00:00:01Z", "dddd"), // 1 token
            mem("big-new", "2026-01-01T00:00:02Z", "eeeeeeeeeeee"), // 3 tokens
        ];
        // Newest-first priority meets "big-new" first; it exceeds the budget
        // and is dropped, but the cheaper, older candidate still fits.
        let (kept, report) = budget_recall(mems, 2);
        assert_eq!(contents(&kept), ["dddd"]);
        assert_eq!(report.dropped_ids, ["big-new"]);
        assert_eq!(report.estimated_tokens, 1);
    }

    #[test]
    fn budget_recall_of_empty_input_is_empty_not_an_error() {
        let (kept, report) = budget_recall(Vec::new(), 10);
        assert!(kept.is_empty());
        assert_eq!(report.included, 0);
        assert_eq!(report.dropped, 0);
        assert_eq!(report.estimated_tokens, 0);
        assert_eq!(report.channels[&"recency"], 0);
    }

    #[test]
    fn budget_search_preserves_rank_order_and_lists_drops_in_drop_order() {
        let mems = vec![
            mem("r1", "2026-01-01T00:00:03Z", "aaaa"), // 1 token, rank 0
            mem("r2", "2026-01-01T00:00:01Z", "bbbbbbbb"), // 2 tokens, rank 1
            mem("r3", "2026-01-01T00:00:02Z", "cccc"), // 1 token, rank 2
        ];
        let (kept, report) = budget_search(mems.clone(), 2);
        assert_eq!(
            contents(&kept),
            ["aaaa", "cccc"],
            "rank order is priority and output order; the middle item is \
             dropped and the later, cheaper one still fits"
        );
        assert_eq!(report.dropped_ids, ["r2"]);
        assert_eq!(report.channels[&"fts"], 3);

        let (kept, report) = budget_search(mems, 1);
        assert_eq!(contents(&kept), ["aaaa"]);
        assert_eq!(
            report.dropped_ids,
            ["r2", "r3"],
            "dropped ids appear in the order they were dropped"
        );
    }

    #[test]
    fn budget_report_sums_only_included_and_accounts_for_every_input() {
        let mems = vec![
            mem("m1", "2026-01-01T00:00:01Z", "aaaaaaaa"), // 2 tokens
            mem("m2", "2026-01-01T00:00:02Z", "bbbb"),     // 1 token
            mem("m3", "2026-01-01T00:00:03Z", "cccc"),     // 1 token
        ];
        let input_len = mems.len();
        let (kept, report) = budget_recall(mems, 2);
        assert_eq!(contents(&kept), ["bbbb", "cccc"]);
        assert_eq!(
            report.estimated_tokens, 2,
            "estimated_tokens counts included items only"
        );
        assert_eq!(report.requested_tokens, 2);
        assert_eq!(report.estimator, ESTIMATOR);
        assert_eq!(
            report.included + report.dropped,
            input_len,
            "every input item is either included or dropped"
        );
    }
}

// Rust guideline compliant 2026-05-18
