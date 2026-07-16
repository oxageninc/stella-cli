//! Lexical mining helpers shared by the rules and skills miners
//! ([`crate::rules`], [`crate::skills`]). Both cluster free-text
//! observations, pick a stable representative wording, and mint
//! deterministic `<slug>-<hash8>` ids — one home so a stopword, similarity,
//! or slug tweak can never land in one miner and silently miss the other.
//! (These started as two byte-identical private copies; the divergence risk
//! is why they were merged.)

use std::collections::{HashMap, HashSet};

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "to", "of", "in", "on", "for", "with", "is", "are", "be",
    "this", "that", "it", "as", "at", "by", "from", "into", "we", "you", "i", "was", "were", "has",
    "have", "had", "not", "but", "if", "so", "then", "than", "when", "where", "which", "will",
    "would", "should", "did",
];

/// Split text into lowercased, de-stopped terms (>2 chars) for lexical
/// scoring/clustering (TS: `terms`).
pub(crate) fn terms(text: &str) -> Vec<String> {
    let stopwords: HashSet<&str> = STOPWORDS.iter().copied().collect();
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            if current.len() > 2 && !stopwords.contains(current.as_str()) {
                out.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if current.len() > 2 && !stopwords.contains(current.as_str()) {
        out.push(current);
    }
    out
}

/// Jaccard similarity of two term sets — 0 when either is empty (TS:
/// `jaccard`).
pub(crate) fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Filesystem/id-safe slug: lowercase, alnum + dashes, capped short.
/// `fallback` names the artifact kind when the text slugs to nothing
/// (`"lesson"` for rules, `"skill"` for skills) (TS: `slugify`).
pub(crate) fn slugify(text: &str, fallback: &str) -> String {
    let mut collapsed = String::new();
    let mut prev_dash = false;
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            collapsed.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            collapsed.push('-');
            prev_dash = true;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    let truncated: String = trimmed.chars().take(40).collect();
    let truncated = truncated.trim_end_matches('-');
    if truncated.is_empty() {
        fallback.to_string()
    } else {
        truncated.to_string()
    }
}

/// Short deterministic content hash (FNV-1a 64-bit, lower 32 bits as 8 hex
/// chars) so re-mining the same data yields the same candidate id (TS:
/// `hash8`, backed by `sha1`). Not cryptographic — this is purely a stable
/// id, not an integrity check, and no hash crate is a workspace dependency;
/// FNV-1a is dependency-free and deterministic, which is all this needs.
pub(crate) fn hash8(text: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{:08x}", (hash & 0xffff_ffff) as u32)
}

/// The cluster's most representative wording: the most-repeated exact
/// text, longest wins ties. `None` only for an empty cluster, which the
/// miners never construct. `text` projects each observation's wording (TS:
/// `representativeText`).
pub(crate) fn representative_text<T>(cluster: &[T], text: impl Fn(&T) -> &str) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for o in cluster {
        *counts.entry(text(o)).or_insert(0) += 1;
    }
    let mut best = text(cluster.first()?);
    let mut best_count = 0usize;
    for (candidate, count) in &counts {
        // Most-frequent, then longest, then lexically smallest. The final
        // lexical tiebreak is load-bearing: without it, two equal-count,
        // equal-length texts resolve in HashMap iteration order (randomized
        // per process), so re-mining the same observations could pick a
        // different representative and mint a duplicate `<slug>-<hash>` file.
        let better = *count > best_count
            || (*count == best_count && candidate.len() > best.len())
            || (*count == best_count && candidate.len() == best.len() && *candidate < best);
        if better {
            best = candidate;
            best_count = *count;
        }
    }
    Some(best.to_string())
}

/// `true` when any existing artifact already says essentially the same
/// thing — the candidate's terms overlap one of the `haystacks` past
/// `min_similarity`. Each miner builds its own haystack (a rule's text; a
/// skill's name+description+body) (TS: `alreadyCaptured`).
pub(crate) fn already_captured(
    text: &str,
    haystacks: impl IntoIterator<Item = String>,
    min_similarity: f64,
) -> bool {
    let t: HashSet<String> = terms(text).into_iter().collect();
    haystacks.into_iter().any(|haystack| {
        let ht: HashSet<String> = terms(&haystack).into_iter().collect();
        jaccard(&t, &ht) >= min_similarity
    })
}

/// Greedy single-pass clustering: each observation joins the first cluster
/// whose *first* observation's term set overlaps enough with it, else
/// starts a new one. `O(n × clusters)` — fine at CLI-local data volumes
/// (TS: the clustering loop inside `mineCandidates`).
pub(crate) fn cluster_observations<T>(
    observations: Vec<T>,
    min_similarity: f64,
    text: impl Fn(&T) -> &str,
) -> Vec<Vec<T>> {
    let mut clusters: Vec<Vec<T>> = Vec::new();
    for obs in observations {
        let obs_terms: HashSet<String> = terms(text(&obs)).into_iter().collect();
        let home = clusters.iter().position(|c| {
            let head_terms: HashSet<String> = terms(text(&c[0])).into_iter().collect();
            jaccard(&obs_terms, &head_terms) >= min_similarity
        });
        match home {
            Some(idx) => clusters[idx].push(obs),
            None => clusters.push(vec![obs]),
        }
    }
    clusters
}
