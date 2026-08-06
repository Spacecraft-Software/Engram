// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Removing credentials from text on its way into the store.
//!
//! `remember` is a model deliberately choosing a sentence. Ingest is engram
//! copying whatever a session contained — which is a materially different
//! privacy posture, because a session contains everything the user pasted.
//! The store is shared by every harness on the machine, several of which are
//! cloud-hosted models, so a secret that lands here has effectively been
//! handed to all of them.
//!
//! This is a **best-effort net, not a guarantee.** It catches the common
//! shapes of machine-issued credentials, which are the ones that turn up in
//! transcripts by accident. It cannot catch a password typed in prose. The
//! primary defense is still the default filtering in [`super`]: tool payloads
//! never enter, and tool payloads are where file contents and command output
//! live.
//!
//! Redaction is length-reducing and idempotent — running it twice produces
//! the same text, and a redacted marker is never itself a match.

use serde::Serialize;
use std::collections::BTreeMap;

/// How many secrets of each kind were replaced.
///
/// A `BTreeMap` so the report is ordered deterministically: the ingest
/// envelope must not shuffle between runs.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(transparent)]
pub struct Redactions(pub BTreeMap<&'static str, usize>);

impl Redactions {
    fn bump(&mut self, kind: &'static str) {
        *self.0.entry(kind).or_insert(0) += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Accumulates another scrub's counts.
    pub fn merge(&mut self, other: &Redactions) {
        for (kind, count) in &other.0 {
            *self.0.entry(kind).or_insert(0) += count;
        }
    }
}

/// A credential shape recognized by its prefix plus a run of token characters.
struct PrefixRule {
    kind: &'static str,
    prefix: &'static str,
    /// Shortest token body that counts as a match. Set per-rule so a bare
    /// mention of the prefix in prose ("keys start with sk-") is not redacted.
    min_body: usize,
}

/// Prefix-shaped credentials. Ordered longest-prefix-first so `sk-ant-` is
/// attributed to Anthropic rather than to the generic `sk-` rule.
const PREFIX_RULES: &[PrefixRule] = &[
    PrefixRule {
        kind: "anthropic-key",
        prefix: "sk-ant-",
        min_body: 16,
    },
    PrefixRule {
        kind: "openai-key",
        prefix: "sk-proj-",
        min_body: 16,
    },
    PrefixRule {
        kind: "github-token",
        prefix: "ghp_",
        min_body: 16,
    },
    PrefixRule {
        kind: "github-token",
        prefix: "gho_",
        min_body: 16,
    },
    PrefixRule {
        kind: "github-token",
        prefix: "ghs_",
        min_body: 16,
    },
    PrefixRule {
        kind: "github-token",
        prefix: "github_pat_",
        min_body: 16,
    },
    PrefixRule {
        kind: "aws-access-key",
        prefix: "AKIA",
        min_body: 12,
    },
    PrefixRule {
        kind: "aws-access-key",
        prefix: "ASIA",
        min_body: 12,
    },
    PrefixRule {
        kind: "slack-token",
        prefix: "xoxb-",
        min_body: 12,
    },
    PrefixRule {
        kind: "slack-token",
        prefix: "xoxp-",
        min_body: 12,
    },
    PrefixRule {
        kind: "google-api-key",
        prefix: "AIza",
        min_body: 30,
    },
    PrefixRule {
        kind: "context7-key",
        prefix: "ctx7sk-",
        min_body: 16,
    },
    PrefixRule {
        kind: "api-key",
        prefix: "sk-",
        min_body: 20,
    },
];

/// Characters that continue a credential token.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// The placeholder a redacted secret becomes.
fn marker(kind: &str) -> String {
    format!("[redacted:{kind}]")
}

/// Replaces credential-shaped substrings and reports what was replaced.
pub fn scrub(text: &str, found: &mut Redactions) -> String {
    let text = scrub_private_keys(text, found);
    let text = scrub_bearer(&text, found);
    scrub_prefixes(&text, found)
}

/// PEM private key blocks, replaced whole. Done first: a key body could
/// otherwise contain something a narrower rule would nibble at.
fn scrub_private_keys(text: &str, found: &mut Redactions) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END_MARK: &str = "-----END ";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find(BEGIN) {
        // Only PEM headers that actually name a private key.
        let header_end = match rest[start..]
            .find("-----\n")
            .or_else(|| rest[start..].find("----- "))
        {
            Some(i) => start + i,
            None => break,
        };
        let header = &rest[start..header_end];
        if !header.contains("PRIVATE KEY") {
            out.push_str(&rest[..header_end + 5]);
            rest = &rest[header_end + 5..];
            continue;
        }
        let after = &rest[header_end..];
        let Some(end_rel) = after.find(END_MARK) else {
            // Unterminated block: drop the remainder rather than leave a key
            // body behind on the assumption it will be closed later.
            out.push_str(&rest[..start]);
            out.push_str(&marker("private-key"));
            found.bump("private-key");
            return out;
        };
        let tail = &after[end_rel..];
        let close = tail
            .find("-----\n")
            .or_else(|| tail.find('\n'))
            .map_or(tail.len(), |i| {
                // Include the closing dashes themselves.
                tail[i..].find('\n').map_or(tail.len(), |_| i + 5)
            });
        out.push_str(&rest[..start]);
        out.push_str(&marker("private-key"));
        found.bump("private-key");
        rest = &tail[close.min(tail.len())..];
    }
    out.push_str(rest);
    out
}

/// `Authorization: Bearer <token>` and bare `Bearer <token>`.
fn scrub_bearer(text: &str, found: &mut Redactions) -> String {
    const BEARER: &str = "Bearer ";
    /// A bearer token shorter than this is more likely prose than a secret.
    const MIN_TOKEN: usize = 12;

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(BEARER) {
        let after = &rest[i + BEARER.len()..];
        let body: String = after
            .chars()
            .take_while(|c| is_token_char(*c) || *c == '.')
            .collect();
        out.push_str(&rest[..i]);
        if body.len() >= MIN_TOKEN {
            out.push_str(BEARER);
            out.push_str(&marker("bearer-token"));
            found.bump("bearer-token");
            rest = &after[body.len()..];
        } else {
            out.push_str(BEARER);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Prefix-shaped credentials.
fn scrub_prefixes(text: &str, found: &mut Redactions) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    'outer: loop {
        // Earliest match across all rules, longest prefix winning a tie, so
        // `sk-ant-` beats the generic `sk-` at the same position.
        let mut best: Option<(usize, &PrefixRule, usize)> = None;
        for rule in PREFIX_RULES {
            let Some(i) = rest.find(rule.prefix) else {
                continue;
            };
            let after = &rest[i + rule.prefix.len()..];
            let body_len = after.chars().take_while(|c| is_token_char(*c)).count();
            if body_len < rule.min_body {
                continue;
            }
            let better = match best {
                None => true,
                Some((bi, brule, _)) => {
                    i < bi || (i == bi && rule.prefix.len() > brule.prefix.len())
                }
            };
            if better {
                best = Some((i, rule, body_len));
            }
        }

        match best {
            Some((i, rule, body_len)) => {
                out.push_str(&rest[..i]);
                out.push_str(&marker(rule.kind));
                found.bump(rule.kind);
                rest = &rest[i + rule.prefix.len() + body_len..];
            }
            None => break 'outer,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrubbed(text: &str) -> (String, Redactions) {
        let mut found = Redactions::default();
        let out = scrub(text, &mut found);
        (out, found)
    }

    #[test]
    fn redacts_prefix_shaped_credentials() {
        let cases = [
            ("sk-ant-api03-AAAAAAAAAAAAAAAAAAAA", "anthropic-key"),
            ("ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "github-token"),
            ("AKIAIOSFODNN7EXAMPLE", "aws-access-key"),
            ("xoxb-1111111111-abcdefghijkl", "slack-token"),
            ("ctx7sk-aaaaaaaaaaaaaaaaaaaa", "context7-key"),
        ];
        for (secret, kind) in cases {
            let (out, found) = scrubbed(&format!("the key is {secret} ok"));
            assert!(!out.contains(secret), "{kind} survived: {out}");
            assert_eq!(found.0.get(kind), Some(&1), "{kind} not counted in {out:?}");
            assert!(out.starts_with("the key is ") && out.ends_with(" ok"));
        }
    }

    #[test]
    fn longest_prefix_wins() {
        let (_, found) = scrubbed("sk-ant-api03-AAAAAAAAAAAAAAAAAAAA");
        assert_eq!(found.0.get("anthropic-key"), Some(&1));
        assert_eq!(
            found.0.get("api-key"),
            None,
            "generic rule must not also fire"
        );
    }

    #[test]
    fn short_lookalikes_in_prose_are_left_alone() {
        let (out, found) = scrubbed("Anthropic keys start with sk- and GitHub with ghp_.");
        assert_eq!(out, "Anthropic keys start with sk- and GitHub with ghp_.");
        assert!(found.is_empty());
    }

    #[test]
    fn redacts_bearer_tokens_but_not_the_word() {
        let (out, found) = scrubbed("Authorization: Bearer abcdefghijklmnopqrstuvwxyz");
        assert_eq!(out, "Authorization: Bearer [redacted:bearer-token]");
        assert_eq!(found.0.get("bearer-token"), Some(&1));

        let (out, found) = scrubbed("Bearer with it for a moment");
        assert_eq!(out, "Bearer with it for a moment");
        assert!(found.is_empty());
    }

    #[test]
    fn redacts_private_key_blocks_whole() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\nMORESECRET\n-----END OPENSSH PRIVATE KEY-----\n";
        let (out, found) = scrubbed(&format!("here it is:\n{pem}done"));
        assert!(!out.contains("MORESECRET"), "key body survived: {out}");
        assert!(!out.contains("b3BlbnNzaC1rZXktdjEAAAAA"));
        assert_eq!(found.0.get("private-key"), Some(&1));
        assert!(out.starts_with("here it is:"));
    }

    #[test]
    fn an_unterminated_private_key_drops_the_tail() {
        let (out, found) = scrubbed("start\n-----BEGIN RSA PRIVATE KEY-----\nSECRETBODY");
        assert!(!out.contains("SECRETBODY"));
        assert_eq!(found.0.get("private-key"), Some(&1));
    }

    /// A certificate is not a secret and must survive.
    #[test]
    fn public_pem_blocks_are_untouched() {
        let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        let (out, found) = scrubbed(pem);
        assert_eq!(out, pem);
        assert!(found.is_empty());
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = scrubbed("key sk-ant-api03-AAAAAAAAAAAAAAAAAAAA here").0;
        let twice = scrubbed(&once).0;
        assert_eq!(once, twice, "a redaction marker must not itself match");
    }

    #[test]
    fn clean_text_is_returned_unchanged() {
        let text = "Decided: the reader streams line by line. See src/transcript/mod.rs.";
        let (out, found) = scrubbed(text);
        assert_eq!(out, text);
        assert!(found.is_empty());
    }
}

// Rust guideline compliant 2026-05-18
