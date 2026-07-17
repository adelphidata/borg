//! Candidate selectors for shadow-mode skill selection.
//!
//! Today every enabled skill's metadata is injected each turn. These selectors
//! model what a relevance-gated injector *would* pick, so the methods can be
//! compared on real traffic before any of them is allowed to gate anything.
//! Nothing here changes what the agent sees — [`super::context`] only logs the
//! outcomes.
//!
//! Every selector is bounded (query, document, and candidate-set size) and
//! deterministic: equal scores resolve by name then id, so two runs over the
//! same catalog in a different input order agree.

use std::collections::{HashMap, HashSet};

/// Skill metadata a selector searches. `id` indexes the caller's slice and is
/// what [`SelectorOutcome`] reports back.
#[derive(Debug, Clone, Copy)]
pub(super) struct SkillSelectionDoc<'a> {
    pub id: usize,
    pub name: &'a str,
    pub triggers: &'a [String],
    pub description: &'a str,
}

/// One selection method's verdict for a turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SelectorOutcome {
    /// Selected doc ids, most relevant first.
    pub candidate_ids: Vec<usize>,
    /// Query terms that survived stop-word filtering and bounding.
    pub query_term_count: usize,
    /// The query hit [`MAX_QUERY_BYTES`] or [`MAX_QUERY_TERMS`].
    pub query_truncated: bool,
    /// The catalog hit [`MAX_CANDIDATES`].
    pub candidate_set_truncated: bool,
}

/// A shadow selection method. Implementations must be pure and cheap: this runs
/// on every turn that has DEBUG logging on, alongside work the budget loop is
/// already doing.
pub(super) trait ShadowSkillSelector: Sync {
    /// Stable, low-cardinality identifier for metrics.
    fn method(&self) -> &'static str;
    /// Rank `docs` against `query`, returning at most `limit` candidates.
    fn select(&self, query: &str, docs: &[SkillSelectionDoc<'_>], limit: usize) -> SelectorOutcome;
}

/// Every method evaluated per turn in shadow mode.
pub(super) const SHADOW_SELECTORS: &[&dyn ShadowSkillSelector] = &[
    &WeightedLexicalSelector,
    &FieldedBm25Selector,
    &MultiQueryLexicalSelector,
];

const MAX_QUERY_BYTES: usize = 4 * 1024;
const MAX_QUERY_TERMS: usize = 64;
const MAX_DOC_BYTES: usize = 4 * 1024;
const MAX_DOC_TERMS: usize = 256;
const MAX_CANDIDATES: usize = 1_000;
const MAX_RESULTS: usize = 50;
/// Distinct readings of a compound query, including the full query itself.
const MAX_QUERY_VIEWS: usize = 8;

/// Field weights for [`FieldedBm25Selector`], in [`FIELD_NAME`]/[`FIELD_TRIGGERS`]/
/// [`FIELD_DESCRIPTION`] order. A name match is the strongest signal a skill is
/// what the user asked for; triggers are curated task phrases; the description
/// is prose and matches incidentally.
const FIELD_WEIGHTS: [f64; 3] = [8.0, 4.0, 1.0];
const FIELD_NAME: usize = 0;
const FIELD_TRIGGERS: usize = 1;
const FIELD_DESCRIPTION: usize = 2;

/// Term-frequency saturation point.
const K1: f64 = 1.2;
/// Field-length normalization strength.
const B: f64 = 0.75;

/// Dropped from queries and documents alike: they appear in nearly every
/// request, so they carry no signal about *which* skill is wanted.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "for", "from", "how", "in", "is", "it",
    "me", "my", "of", "on", "or", "please", "that", "the", "this", "to", "use", "we", "what",
    "when", "where", "which", "with", "you", "your",
];

/// Phrases that join independent asks in one message. Longest first so
/// `" and then "` wins over `" and "` at the same position.
const CONNECTORS: &[&str] = &[" and then ", " and ", " then ", " also "];

/// Truncate to at most `max_bytes` on a char boundary, reporting whether
/// anything was cut.
fn bounded(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// Split into lowercase alphanumeric terms, dropping single characters and stop
/// words.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
        .map(str::to_lowercase)
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect()
}

/// Bounded, deduplicated query terms. Order is preserved; duplicates carry no
/// extra weight in a query this short.
fn query_terms(query: &str) -> (Vec<String>, bool) {
    let (query, bytes_truncated) = bounded(query, MAX_QUERY_BYTES);
    let mut seen = HashSet::new();
    let mut terms: Vec<String> = tokenize(query)
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect();
    let terms_truncated = terms.len() > MAX_QUERY_TERMS;
    terms.truncate(MAX_QUERY_TERMS);
    (terms, bytes_truncated || terms_truncated)
}

/// Bounded terms for one document field.
fn field_terms(text: &str) -> Vec<String> {
    let (text, _) = bounded(text, MAX_DOC_BYTES);
    let mut terms = tokenize(text);
    terms.truncate(MAX_DOC_TERMS);
    terms
}

/// The lexical scoring that also orders budget consumption in
/// [`super::context`]. Trigger phrases dominate, then whole-phrase matches on
/// any metadata term.
pub(super) fn lexical_relevance(
    name: &str,
    description: &str,
    triggers: &[String],
    request: &str,
) -> u32 {
    if request.trim().is_empty() {
        return 0;
    }

    let request = request.to_lowercase();
    let mut score = 0;

    for trigger in triggers {
        let trigger = trigger.trim().to_lowercase();
        if trigger.is_empty() {
            continue;
        }
        if request.contains(&trigger) {
            score += 10;
        } else if trigger
            .split_whitespace()
            .all(|word| !word.is_empty() && request.contains(word))
        {
            score += 5;
        }
    }

    let mut terms = vec![name.to_lowercase(), description.to_lowercase()];
    terms.extend(triggers.iter().map(|t| t.to_lowercase()));
    for term in terms {
        if term.is_empty() {
            continue;
        }
        if request.contains(&term) {
            score += 3;
        }
    }

    score
}

/// Sort helper shared by the ranked selectors: descending score, then name,
/// then id — so ties never depend on input order.
fn sort_ranked(scored: &mut [(usize, &str, f64)]) {
    scored.sort_by(|a, b| {
        b.2.total_cmp(&a.2)
            .then_with(|| a.1.cmp(b.1))
            .then_with(|| a.0.cmp(&b.0))
    });
}

/// Today's scorer, behind the trait so it can be compared against the others.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct WeightedLexicalSelector;

impl ShadowSkillSelector for WeightedLexicalSelector {
    fn method(&self) -> &'static str {
        "weighted_lexical_v1"
    }

    fn select(&self, query: &str, docs: &[SkillSelectionDoc<'_>], limit: usize) -> SelectorOutcome {
        let (bounded_query, _) = bounded(query, MAX_QUERY_BYTES);
        // Take the flag from `query_terms`, which reports both byte and
        // term-count truncation — the same source the other methods use. Using
        // `bounded`'s byte-only flag here would report a phantom divergence
        // between methods on a query that only busted the term cap.
        let (terms, query_truncated) = query_terms(query);
        let candidate_set_truncated = docs.len() > MAX_CANDIDATES;
        let limit = limit.min(MAX_RESULTS);

        let mut scored: Vec<(usize, &str, f64)> = docs
            .iter()
            .take(MAX_CANDIDATES)
            .filter_map(|d| {
                let score =
                    lexical_relevance(d.name, d.description, d.triggers, bounded_query) as f64;
                (score > 0.0).then_some((d.id, d.name, score))
            })
            .collect();
        sort_ranked(&mut scored);
        scored.truncate(limit);

        SelectorOutcome {
            candidate_ids: scored.into_iter().map(|(id, _, _)| id).collect(),
            query_term_count: terms.len(),
            query_truncated,
            candidate_set_truncated,
        }
    }
}

/// One document's per-field terms, prepared once per selection.
struct PreparedDoc<'a> {
    id: usize,
    name: &'a str,
    fields: [Vec<String>; 3],
}

impl<'a> PreparedDoc<'a> {
    fn new(doc: &SkillSelectionDoc<'a>) -> Self {
        // Bound each trigger before joining so one pathological trigger cannot
        // consume the whole field budget.
        let triggers = doc
            .triggers
            .iter()
            .map(|t| bounded(t, MAX_DOC_BYTES).0)
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            id: doc.id,
            name: doc.name,
            fields: [
                field_terms(doc.name),
                field_terms(&triggers),
                field_terms(doc.description),
            ],
        }
    }
}

/// BM25F over name, triggers, and description.
///
/// Weighted term frequencies are pooled across fields before saturation, so a
/// term in the name outranks the same term buried in prose, and rare terms
/// outrank common ones via IDF.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct FieldedBm25Selector;

impl ShadowSkillSelector for FieldedBm25Selector {
    fn method(&self) -> &'static str {
        "fielded_bm25_v1"
    }

    fn select(&self, query: &str, docs: &[SkillSelectionDoc<'_>], limit: usize) -> SelectorOutcome {
        let (terms, query_truncated) = query_terms(query);
        let candidate_set_truncated = docs.len() > MAX_CANDIDATES;
        let limit = limit.min(MAX_RESULTS);

        let outcome_shell = SelectorOutcome {
            query_term_count: terms.len(),
            query_truncated,
            candidate_set_truncated,
            ..Default::default()
        };
        if terms.is_empty() || limit == 0 {
            return outcome_shell;
        }

        let prepared: Vec<PreparedDoc<'_>> = docs
            .iter()
            .take(MAX_CANDIDATES)
            .map(PreparedDoc::new)
            .collect();
        if prepared.is_empty() {
            return outcome_shell;
        }

        let n = prepared.len() as f64;
        let mut avg_field_len = [0.0f64; 3];
        for (f, avg) in avg_field_len.iter_mut().enumerate() {
            let total: usize = prepared.iter().map(|d| d.fields[f].len()).sum();
            *avg = (total as f64 / n).max(f64::EPSILON);
        }

        // Document frequency counts a term once per doc, across all fields.
        let mut df: HashMap<&str, usize> = HashMap::new();
        for doc in &prepared {
            let mut seen: HashSet<&str> = HashSet::new();
            for field in &doc.fields {
                for term in field {
                    seen.insert(term.as_str());
                }
            }
            for term in seen {
                *df.entry(term).or_insert(0) += 1;
            }
        }

        let mut scored: Vec<(usize, &str, f64)> = prepared
            .iter()
            .filter_map(|doc| {
                let mut score = 0.0;
                for term in &terms {
                    let mut weighted_tf = 0.0;
                    for f in [FIELD_NAME, FIELD_TRIGGERS, FIELD_DESCRIPTION] {
                        let tf = doc.fields[f].iter().filter(|t| *t == term).count() as f64;
                        if tf == 0.0 {
                            continue;
                        }
                        let norm = 1.0 - B + B * (doc.fields[f].len() as f64 / avg_field_len[f]);
                        weighted_tf += FIELD_WEIGHTS[f] * tf / norm;
                    }
                    if weighted_tf == 0.0 {
                        continue;
                    }
                    let doc_freq = *df.get(term.as_str()).unwrap_or(&0) as f64;
                    let idf = (1.0 + (n - doc_freq + 0.5) / (doc_freq + 0.5)).ln();
                    score += idf * (weighted_tf * (K1 + 1.0)) / (weighted_tf + K1);
                }
                (score > 0.0).then_some((doc.id, doc.name, score))
            })
            .collect();
        sort_ranked(&mut scored);
        scored.truncate(limit);

        SelectorOutcome {
            candidate_ids: scored.into_iter().map(|(id, _, _)| id).collect(),
            ..outcome_shell
        }
    }
}

/// Byte range of the leftmost connector phrase in `s`, if any. Connectors are
/// ASCII, so matching case-insensitively on bytes keeps the returned indices
/// valid for slicing `s`.
fn find_connector(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    for i in 0..bytes.len() {
        if !s.is_char_boundary(i) {
            continue;
        }
        for conn in CONNECTORS {
            let end = i + conn.len();
            if end <= bytes.len()
                && s.is_char_boundary(end)
                && bytes[i..end].eq_ignore_ascii_case(conn.as_bytes())
            {
                return Some((i, end));
            }
        }
    }
    None
}

/// Distinct readings of `query`: the full text first, then each sentence and
/// connector-delimited clause. A single-clause query yields exactly one view.
pub(super) fn query_views(query: &str) -> Vec<&str> {
    let (query, _) = bounded(query, MAX_QUERY_BYTES);
    let mut views = vec![query.trim()];

    let mut clauses: Vec<&str> = Vec::new();
    for sentence in query.split(['\n', '\r', '.', '!', '?', ';']) {
        let mut rest = sentence;
        loop {
            match find_connector(rest) {
                Some((start, end)) => {
                    clauses.push(&rest[..start]);
                    rest = &rest[end..];
                }
                None => {
                    clauses.push(rest);
                    break;
                }
            }
        }
    }

    for clause in clauses {
        if views.len() >= MAX_QUERY_VIEWS {
            break;
        }
        let clause = clause.trim();
        if clause.chars().count() < 2 || views.contains(&clause) {
            continue;
        }
        views.push(clause);
    }
    views
}

/// Merge state for one doc across the query's views.
struct MergedCandidate {
    id: usize,
    best_rank: usize,
    full_query_rank: Option<usize>,
    view_count: usize,
    first_view: usize,
}

/// Lexical selection run over each clause of a compound request, merged by best
/// rank.
///
/// "Deploy the app and then check the weather" ranks poorly as one string —
/// neither skill matches the whole thing. Scoring each clause separately lets
/// both surface, while the full-query rank still breaks ties in favor of skills
/// that match the request as a whole.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MultiQueryLexicalSelector;

impl ShadowSkillSelector for MultiQueryLexicalSelector {
    fn method(&self) -> &'static str {
        "multi_query_lexical_v1"
    }

    fn select(&self, query: &str, docs: &[SkillSelectionDoc<'_>], limit: usize) -> SelectorOutcome {
        let views = query_views(query);
        let full = WeightedLexicalSelector.select(query, docs, limit);
        // Not a compound query: identical to the plain lexical method.
        if views.len() <= 1 {
            return full;
        }

        let limit = limit.min(MAX_RESULTS);
        let mut merged: Vec<MergedCandidate> = Vec::new();
        let mut query_truncated = full.query_truncated;
        let mut candidate_set_truncated = full.candidate_set_truncated;

        for (view_idx, view) in views.iter().enumerate() {
            let outcome = if view_idx == 0 {
                full.clone()
            } else {
                WeightedLexicalSelector.select(view, docs, limit)
            };
            query_truncated |= outcome.query_truncated;
            candidate_set_truncated |= outcome.candidate_set_truncated;

            for (rank, id) in outcome.candidate_ids.into_iter().enumerate() {
                match merged.iter_mut().find(|c| c.id == id) {
                    Some(existing) => {
                        existing.best_rank = existing.best_rank.min(rank);
                        existing.view_count += 1;
                        if view_idx == 0 {
                            existing.full_query_rank = Some(rank);
                        }
                    }
                    None => merged.push(MergedCandidate {
                        id,
                        best_rank: rank,
                        full_query_rank: (view_idx == 0).then_some(rank),
                        view_count: 1,
                        first_view: view_idx,
                    }),
                }
            }
        }

        merged.sort_by(|a, b| {
            a.best_rank
                .cmp(&b.best_rank)
                // `None` sorts last: matching the request as a whole is a
                // stronger signal than matching one clause of it.
                .then_with(|| {
                    a.full_query_rank
                        .unwrap_or(usize::MAX)
                        .cmp(&b.full_query_rank.unwrap_or(usize::MAX))
                })
                .then_with(|| b.view_count.cmp(&a.view_count))
                .then_with(|| a.first_view.cmp(&b.first_view))
                .then_with(|| a.id.cmp(&b.id))
        });
        merged.truncate(limit);

        SelectorOutcome {
            candidate_ids: merged.into_iter().map(|c| c.id).collect(),
            query_term_count: full.query_term_count,
            query_truncated,
            candidate_set_truncated,
        }
    }
}
