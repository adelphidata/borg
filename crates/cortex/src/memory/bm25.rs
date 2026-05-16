//! BM25 scoring computed in Rust over a candidate set returned by an FTS
//! MATCH. Turso's `USING fts` index method (Phase 5 migration) replaced the
//! SQLite FTS5 virtual table; the new method doesn't expose a SQL-callable
//! `bm25()`, so ranking moves to the application layer.
//!
//! The corpus statistics (`avgdl`, `doc_freq`) are derived from the
//! post-MATCH candidate set, *not* the full index. This is a deliberate
//! choice: full-index IDF would require a second pass over the entire FTS
//! corpus per query, which defeats the point of the candidate widening.
//! Candidate-corpus IDF is a well-known approximation used by Lucene's
//! `rerank` query and is good enough for the 70/30 hybrid mix downstream.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

/// Term-frequency saturation parameter. Higher values reduce the
/// diminishing-returns curve on repeated terms.
pub const BM25_K1: f64 = 1.2;

/// Length-normalization parameter. 0 disables normalization, 1 fully
/// normalizes by `doc_len / avgdl`.
pub const BM25_B: f64 = 0.75;

/// Per-query corpus statistics, derived from the candidate set returned by
/// the FTS MATCH. `doc_freq` is term → number of candidate docs containing
/// the term.
#[derive(Debug, Clone)]
pub struct CorpusStats {
    /// Number of documents in the candidate corpus.
    pub doc_count: usize,
    /// Average document length (token count) across the corpus.
    pub avgdl: f64,
    /// Document frequency table: term → number of corpus docs containing
    /// at least one occurrence of the term.
    pub doc_freq: HashMap<String, usize>,
}

impl CorpusStats {
    /// Build corpus statistics from the raw document texts. Tokenization
    /// uses the same `\w+` lowercased rule as [`tokenize`].
    pub fn from_docs(docs: &[String]) -> Self {
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        let mut total_len: usize = 0;
        for doc in docs {
            let toks = tokenize(doc);
            total_len += toks.len();
            // Each term contributes at most once to `doc_freq` per document.
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for t in &toks {
                if seen.insert(t.as_str()) {
                    *doc_freq.entry(t.clone()).or_insert(0) += 1;
                }
            }
        }
        let doc_count = docs.len();
        let avgdl = if doc_count == 0 {
            0.0
        } else {
            total_len as f64 / doc_count as f64
        };
        Self {
            doc_count,
            avgdl,
            doc_freq,
        }
    }
}

/// Compute the BM25 score for `doc` against `query_terms` given the
/// candidate-corpus statistics. Returns `0.0` for an empty query, an empty
/// corpus, or when no query term matches.
pub fn bm25_score(query_terms: &[&str], doc: &str, corpus: &CorpusStats) -> f64 {
    if query_terms.is_empty() || corpus.doc_count == 0 {
        return 0.0;
    }
    let doc_tokens = tokenize(doc);
    let doc_len = doc_tokens.len() as f64;
    // Term-frequency table for this doc.
    let mut tf: HashMap<&str, usize> = HashMap::new();
    for t in &doc_tokens {
        *tf.entry(t.as_str()).or_insert(0) += 1;
    }
    let n = corpus.doc_count as f64;
    let avgdl = if corpus.avgdl > 0.0 {
        corpus.avgdl
    } else {
        1.0
    };
    let mut score = 0.0;
    for raw in query_terms {
        let term = raw.to_lowercase();
        let df = *corpus.doc_freq.get(&term).unwrap_or(&0) as f64;
        // Lucene BM25Similarity-style IDF with explicit clamp on the
        // trivial case (df == n): a term present in every candidate doc
        // contributes nothing rather than the small positive value the
        // `+1` smoothing would otherwise yield.
        let idf = if df >= n {
            0.0
        } else {
            ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
        };
        let tf_in_doc = *tf.get(term.as_str()).unwrap_or(&0) as f64;
        if tf_in_doc == 0.0 || idf <= 0.0 {
            continue;
        }
        let denom = tf_in_doc + BM25_K1 * (1.0 - BM25_B + BM25_B * (doc_len / avgdl));
        let numer = tf_in_doc * (BM25_K1 + 1.0);
        score += idf * (numer / denom);
    }
    score
}

/// Score every candidate against `query` and normalize the resulting BM25
/// scores into `[0, 1]` by dividing by the max. Returns one `f32` per input
/// candidate, in the same order. Normalization makes the BM25 component
/// comparable to the `[0, 1]` cosine-similarity component in the hybrid
/// mix. If every score is zero (no query term matched any candidate) the
/// output is all zeros, not NaNs.
pub fn score_candidates(query: &str, candidate_docs: &[&str]) -> Vec<f32> {
    if candidate_docs.is_empty() {
        return Vec::new();
    }
    let query_terms_owned = tokenize(query);
    if query_terms_owned.is_empty() {
        return vec![0.0; candidate_docs.len()];
    }
    let query_terms: Vec<&str> = query_terms_owned.iter().map(String::as_str).collect();
    let docs_owned: Vec<String> = candidate_docs
        .iter()
        .map(std::string::ToString::to_string)
        .collect();
    let corpus = CorpusStats::from_docs(&docs_owned);
    let raw: Vec<f64> = candidate_docs
        .iter()
        .map(|d| bm25_score(&query_terms, d, &corpus))
        .collect();
    let max = raw.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return vec![0.0; candidate_docs.len()];
    }
    raw.into_iter().map(|s| (s / max) as f32).collect()
}

/// Tokenize `s` into lowercase word-character runs (`\w+`). Matches the
/// tokenization used to build `CorpusStats` so scores stay consistent.
pub fn tokenize(s: &str) -> Vec<String> {
    static TOK: LazyLock<Regex> = LazyLock::new(|| {
        // \w is \p{Alphabetic} + digits + underscore in regex with default
        // unicode support, which is enabled by the workspace dep.
        #[allow(clippy::expect_used)]
        Regex::new(r"\w+").expect("static regex compiles")
    });
    TOK.find_iter(s)
        .map(|m| m.as_str().to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(docs: &[&str]) -> (Vec<String>, CorpusStats) {
        let docs_owned: Vec<String> = docs.iter().map(|s| s.to_string()).collect();
        let corpus = CorpusStats::from_docs(&docs_owned);
        (docs_owned, corpus)
    }

    #[test]
    fn bm25_dense_match_outranks_sparse_match() {
        // Three docs share the same vocabulary so every term has identical
        // IDF; only TF/length should differ. The dense doc repeats every
        // query term four times; the sparse doc mentions each query term
        // exactly once but is padded with noise so its length is similar.
        let docs = [
            "rust async tokio rust async tokio rust async tokio rust async tokio",
            "rust async tokio filler filler filler filler filler filler filler filler filler",
            "filler filler filler filler filler filler filler filler filler filler filler filler",
        ];
        let (owned, corpus) = build(&docs);
        let dense = bm25_score(&["rust", "async", "tokio"], &owned[0], &corpus);
        let sparse = bm25_score(&["rust", "async", "tokio"], &owned[1], &corpus);
        let none = bm25_score(&["rust", "async", "tokio"], &owned[2], &corpus);
        assert!(
            dense > sparse,
            "dense ({dense}) should outrank sparse ({sparse})"
        );
        assert!(
            sparse > none,
            "sparse ({sparse}) should outrank none ({none})"
        );
    }

    #[test]
    fn bm25_idf_penalizes_common_terms() {
        // "the" appears in every doc → IDF clamps to 0; "neutrino" appears
        // in exactly one doc → high IDF. A query of ["the", "neutrino"]
        // should give all weight to the rare-term doc, even though both
        // docs contain "the" with similar frequency.
        let docs = [
            "the the the the neutrino neutrino", // doc with rare term
            "the the the the cat cat cat cat",   // doc without
            "the the the the dog dog dog",
            "the the the the bird bird",
        ];
        let (owned, corpus) = build(&docs);
        let rare_present = bm25_score(&["the", "neutrino"], &owned[0], &corpus);
        let rare_absent = bm25_score(&["the", "neutrino"], &owned[1], &corpus);
        // The "the" contribution must be zero on its own (IDF clamp).
        let only_common = bm25_score(&["the"], &owned[1], &corpus);
        assert_eq!(
            only_common, 0.0,
            "a term in every doc must contribute zero (got {only_common})"
        );
        assert!(
            rare_present > rare_absent,
            "doc with rare term ({rare_present}) must score above doc without ({rare_absent})"
        );
    }

    #[test]
    fn bm25_empty_query_returns_zero() {
        let corpus = CorpusStats::from_docs(&["hello world".to_string()]);
        let s = bm25_score(&[], "hello world", &corpus);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn tokenize_lowercases_and_splits() {
        assert_eq!(
            tokenize("Hello, WORLD-foo."),
            vec!["hello".to_string(), "world".to_string(), "foo".to_string()]
        );
    }

    #[test]
    fn corpus_stats_avgdl_matches_token_count() {
        // 2 docs with 3 + 5 = 8 tokens → avgdl = 4.0.
        let stats = CorpusStats::from_docs(&[
            "alpha beta gamma".to_string(),
            "one two three four five".to_string(),
        ]);
        assert_eq!(stats.doc_count, 2);
        assert!((stats.avgdl - 4.0).abs() < 1e-9);
        // Each term appears in exactly one doc.
        assert_eq!(stats.doc_freq.get("alpha"), Some(&1));
        assert_eq!(stats.doc_freq.get("five"), Some(&1));
    }

    #[test]
    fn score_candidates_normalizes_to_unit_max() {
        let docs = [
            "rust async tokio rust async tokio",
            "rust filler filler filler",
            "filler filler filler filler",
        ];
        let scores = score_candidates("rust async tokio", &docs);
        assert_eq!(scores.len(), 3);
        let max = scores.iter().cloned().fold(0.0_f32, f32::max);
        assert!(
            (max - 1.0).abs() < 1e-6,
            "max should normalize to 1.0 (got {max})"
        );
        // Best-matching doc must outscore others.
        assert!(scores[0] > scores[1]);
        assert!(scores[1] > scores[2]);
        // No-match doc must score zero.
        assert_eq!(scores[2], 0.0);
    }

    #[test]
    fn score_candidates_empty_query_returns_zero_vector() {
        let docs = ["rust".to_string()];
        let docs_refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let scores = score_candidates("", &docs_refs);
        assert_eq!(scores, vec![0.0]);
    }

    #[test]
    fn corpus_stats_repeated_term_counts_as_one_doc() {
        // "alpha" appears 3x in one doc — doc_freq must still be 1, not 3.
        let stats = CorpusStats::from_docs(&["alpha alpha alpha".to_string(), "beta".to_string()]);
        assert_eq!(stats.doc_freq.get("alpha"), Some(&1));
    }
}
