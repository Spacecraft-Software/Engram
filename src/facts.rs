// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic fact extraction — pure functions, no database handle and
//! **no LLM, ever**.
//!
//! Facts are an *index over* memories, never a replacement for them: each
//! extracted fact is a verbatim substring of its parent memory's content,
//! stored with a drill-down pointer (`memory_id`) back to the verbatim
//! record. Rewriting a fact ("summarizing" it) is the lossy-extraction trap
//! this module deliberately avoids — what is indexed is exactly what was
//! said.
//!
//! # Algorithm (`deterministic-v1`)
//!
//! 1. Split the content into candidate units: every line, plus — for lines
//!    that split into more than one sentence — each sentence of that line.
//!    A sentence boundary is `". "`, `"! "`, or `"? "` followed by an
//!    uppercase letter (deliberately simple and deterministic).
//! 2. Trim each unit: leading whitespace, one leading bullet marker
//!    (`"- "`, `"* "`, `"• "`), leading whitespace again, and trailing
//!    whitespace. The trimmed unit is the fact text.
//! 3. A unit is a fact iff it starts (case-insensitively) with one of the
//!    [`MARKERS`] — decision/task/constraint phrasing such as `Decided:`,
//!    `TODO`, or `Never `.
//! 4. Require at least [`MIN_FACT_CHARS`] characters after the marker
//!    check; shorter units carry too little to retrieve by.
//! 5. Dedupe exact duplicates, preserving the first occurrence, and cap at
//!    [`MAX_FACTS_PER_MEMORY`] facts per memory — the first eight distinct
//!    facts in document order. Nothing is logged when the cap bites; the
//!    cap itself is the documented behavior.
//!
//! Because every step is a pure function of the content, extraction is
//! deterministic: the same content always yields the same facts, and
//! [`fact_id`] derives the same UUID for the same `(memory, fact)` pair,
//! which is what makes re-extraction idempotent via `INSERT OR REPLACE`.

/// Identifier of this extraction algorithm, stored in `facts.extractor`.
///
/// Bump the suffix if the algorithm changes observably (markers, cap,
/// splitting) so rows from different algorithm generations stay
/// distinguishable.
pub const EXTRACTOR: &str = "deterministic-v1";

/// Case-insensitive prefixes that qualify a unit as a fact.
///
/// Decision markers (`Decided:`, `Chose:`, `Rejected:`), task markers
/// (`TODO`, `FIXME`), caution markers (`Gotcha:`, `Warning:`), and
/// imperative/constraint phrasing (`Never `, `Must `, ...). The trailing
/// space on the bare-word markers prevents prefix accidents ("Mustang"
/// does not match `Must `). All lowercase here; matching lowercases the
/// candidate.
const MARKERS: [&str; 19] = [
    "decided:",
    "decision:",
    "todo",
    "fixme",
    "note:",
    "rule:",
    "fix:",
    "fixed:",
    "chose:",
    "chosen:",
    "rejected:",
    "constraint:",
    "gotcha:",
    "warning:",
    "never ",
    "always ",
    "must ",
    "do not ",
    "don't ",
];

/// Bullet prefixes stripped before the marker check (and from the stored
/// fact — they are list markup, not fact content).
const BULLETS: [&str; 3] = ["- ", "* ", "• "];

/// Minimum fact length in Unicode characters. Shorter marker hits
/// ("TODO x") carry too little content to be worth indexing.
const MIN_FACT_CHARS: usize = 12;

/// Cap on facts per memory: the first eight distinct facts in document
/// order. A memory that is wall-to-wall marker lines is a checklist, and
/// its head entries are index enough — drill-down reaches the rest.
const MAX_FACTS_PER_MEMORY: usize = 8;

/// Extracts the facts of `content` per the `deterministic-v1` algorithm
/// (see the module documentation). Deterministic, order-preserving,
/// deduped, capped at [`MAX_FACTS_PER_MEMORY`].
pub fn extract(content: &str) -> Vec<String> {
    let mut facts: Vec<String> = Vec::new();
    for line in content.lines() {
        // The line itself is always a unit; its sentences are additional
        // units only when the line actually splits — a single-sentence
        // line would just repeat itself.
        let mut units: Vec<&str> = vec![line];
        let sentences = split_sentences(line);
        if sentences.len() > 1 {
            units.extend(sentences);
        }
        for unit in units {
            let fact = trim_unit(unit);
            if !starts_with_marker(fact) {
                continue;
            }
            if fact.chars().count() < MIN_FACT_CHARS {
                continue;
            }
            if facts.iter().any(|existing| existing == fact) {
                continue;
            }
            facts.push(fact.to_string());
            if facts.len() == MAX_FACTS_PER_MEMORY {
                return facts;
            }
        }
    }
    facts
}

/// Deterministic id for a `(memory, fact)` pair: UUID v5 (namespaced SHA-1)
/// over [`uuid::Uuid::NAMESPACE_OID`].
///
/// Same memory + same fact text ⇒ same id, on every machine, forever —
/// re-running extraction upserts in place instead of accumulating
/// duplicates.
pub fn fact_id(memory_id: &str, fact: &str) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("engram-fact:{memory_id}:{fact}").as_bytes(),
    )
    .to_string()
}

/// Leading whitespace, one leading bullet marker, leading whitespace again,
/// trailing whitespace — gone. What remains is the fact text.
fn trim_unit(unit: &str) -> &str {
    let mut trimmed = unit.trim();
    for bullet in BULLETS {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            trimmed = rest.trim_start();
            break;
        }
    }
    trimmed
}

/// Case-insensitive [`MARKERS`] prefix check.
fn starts_with_marker(fact: &str) -> bool {
    let lowered = fact.to_lowercase();
    MARKERS.iter().any(|marker| lowered.starts_with(marker))
}

/// Splits a line at `". "`, `"! "`, or `"? "` followed by an uppercase
/// letter. The punctuation stays with its sentence; the separating space is
/// dropped. A line with no boundary comes back as a single sentence.
fn split_sentences(line: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0usize;
    for (index, ch) in line.char_indices() {
        if !matches!(ch, '.' | '!' | '?') {
            continue;
        }
        let after_punct = index + ch.len_utf8();
        let mut rest = line[after_punct..].chars();
        if rest.next() == Some(' ') && rest.next().is_some_and(char::is_uppercase) {
            sentences.push(&line[start..after_punct]);
            start = after_punct + 1; // the boundary space is one byte
        }
    }
    if start < line.len() {
        sentences.push(&line[start..]);
    }
    sentences
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_is_deterministic_and_ids_are_stable() {
        let content = "Decided: the flange stays synchronous.\n\
                       chatter in between\n\
                       - TODO check the flange torque calibration";
        let first = extract(content);
        let second = extract(content);
        assert_eq!(first, second, "same input must yield the same facts");
        assert_eq!(first.len(), 2);

        let ids: Vec<String> = first.iter().map(|f| fact_id("mem-1", f)).collect();
        let ids_again: Vec<String> = second.iter().map(|f| fact_id("mem-1", f)).collect();
        assert_eq!(ids, ids_again, "same (memory, fact) must yield the same id");
        // Different memory or different fact ⇒ different id.
        assert_ne!(fact_id("mem-1", &first[0]), fact_id("mem-2", &first[0]));
        assert_ne!(fact_id("mem-1", &first[0]), fact_id("mem-1", &first[1]));
        for id in &ids {
            uuid::Uuid::parse_str(id).expect("fact ids are UUIDs");
        }
    }

    #[test]
    fn every_marker_family_is_recognized() {
        for marker in MARKERS {
            let line = format!("{marker} keep the flange torque within spec");
            let facts = extract(&line);
            assert_eq!(facts, vec![line.clone()], "marker {marker:?} must extract");
        }
    }

    #[test]
    fn marker_match_is_case_insensitive() {
        assert_eq!(
            extract("decided: lowercase phrasing still counts"),
            vec!["decided: lowercase phrasing still counts".to_string()]
        );
        assert_eq!(
            extract("NEVER run the pump dry on startup"),
            vec!["NEVER run the pump dry on startup".to_string()]
        );
    }

    #[test]
    fn bullet_markers_are_trimmed_before_the_check_and_from_the_fact() {
        for bullet in BULLETS {
            let line = format!("  {bullet}TODO check the flange torque");
            assert_eq!(
                extract(&line),
                vec!["TODO check the flange torque".to_string()],
                "bullet {bullet:?} must be stripped"
            );
        }
    }

    #[test]
    fn short_units_are_rejected_by_the_12_char_floor() {
        assert!(extract("TODO x").is_empty(), "6 chars is below the floor");
        assert!(
            extract("Never do.").is_empty(),
            "9 chars is below the floor"
        );
        // Exactly 12 characters passes.
        let exactly_12 = "TODO abcdefg";
        assert_eq!(exactly_12.chars().count(), 12);
        assert_eq!(extract(exactly_12), vec![exactly_12.to_string()]);
    }

    #[test]
    fn the_cap_keeps_the_first_eight_distinct_facts() {
        let lines: Vec<String> = (0..12)
            .map(|i| format!("Decided: numbered decision item {i}"))
            .collect();
        let facts = extract(&lines.join("\n"));
        assert_eq!(facts.len(), 8, "capped at eight facts per memory");
        assert_eq!(facts, lines[..8], "the first eight in document order");
    }

    #[test]
    fn exact_duplicates_collapse_to_the_first_occurrence() {
        let content = "Decided: one source of truth.\n\
                       filler\n\
                       Decided: one source of truth.";
        assert_eq!(
            extract(content),
            vec!["Decided: one source of truth.".to_string()]
        );
    }

    #[test]
    fn plain_narrative_yields_nothing() {
        let content = "The crew reviewed the telemetry after lunch. Everything \
                       looked nominal, and the shift handed over without incident. \
                       Nobody raised any concerns about the flange.";
        assert!(extract(content).is_empty(), "no markers, no facts");
    }

    #[test]
    fn marker_sentences_are_extracted_from_multi_sentence_lines() {
        let content = "The nozzle test went fine. Decided: keep the flange torque \
                       at spec. More chatter follows here.";
        assert_eq!(
            extract(content),
            vec!["Decided: keep the flange torque at spec.".to_string()],
            "the marker sentence is pulled out of the surrounding line"
        );
        // The extracted fact is a verbatim substring of the content.
        for fact in extract(content) {
            assert!(content.contains(&fact), "facts are verbatim substrings");
        }
    }
}

// Rust guideline compliant 2026-05-18
