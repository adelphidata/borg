//! Maximal Marginal Relevance (MMR) re-ranking to reduce redundancy in search results.
//!
//! After hybrid search produces scored results, MMR iteratively selects items that
//! balance relevance with diversity using Jaccard text similarity.

use std::collections::HashSet;

/// Tokenize text into lowercase word tokens for similarity comparison.
fn tokenize(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Jaccard similarity between two texts (tokenized as word sets).
/// Returns 0.0 for disjoint, 1.0 for identical token sets.
#[cfg(test)]
fn jaccard_similarity(a: &str, b: &str) -> f32 {
    let set_a = tokenize(a);
    let set_b = tokenize(b);
    jaccard_sim_sets(&set_a, &set_b)
}

/// Jaccard similarity between pre-tokenized sets.
fn jaccard_sim_sets(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let intersection = a.intersection(b).count();
    let union = a.union(b).count();

    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

/// Apply MMR re-ranking to search results.
///
/// # Arguments
/// * `results` - Slice of (original_index, relevance_score, snippet_text)
/// * `lambda` - Tradeoff: 1.0 = pure relevance, 0.0 = pure diversity
/// * `max_results` - Maximum number of results to return
///
/// # Returns
/// Reordered original indices.
pub fn mmr_rerank(results: &[(usize, f32, &str)], lambda: f32, max_results: usize) -> Vec<usize> {
    if results.is_empty() {
        return Vec::new();
    }

    let lambda = lambda.clamp(0.0, 1.0);
    let limit = max_results.min(results.len());

    // Normalize relevance scores to [0, 1]
    let max_score = results
        .iter()
        .map(|r| r.1)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_score = results.iter().map(|r| r.1).fold(f32::INFINITY, f32::min);
    let range = max_score - min_score;
    let normalize = |s: f32| -> f32 {
        if range > 0.0 {
            (s - min_score) / range
        } else {
            1.0
        }
    };

    // Pre-tokenize all snippets
    let token_sets: Vec<HashSet<String>> = results.iter().map(|r| tokenize(r.2)).collect();

    let mut selected: Vec<usize> = Vec::with_capacity(limit); // indices into `results`
    let mut remaining: HashSet<usize> = (0..results.len()).collect();

    while selected.len() < limit && !remaining.is_empty() {
        let mut best_idx = None;
        let mut best_mmr = f32::NEG_INFINITY;

        for &candidate in &remaining {
            let relevance = normalize(results[candidate].1);

            // Max similarity to any already-selected item
            let max_sim = if selected.is_empty() {
                0.0
            } else {
                selected
                    .iter()
                    .map(|&sel| jaccard_sim_sets(&token_sets[candidate], &token_sets[sel]))
                    .fold(0.0f32, f32::max)
            };

            let mmr_score = lambda * relevance - (1.0 - lambda) * max_sim;

            if mmr_score > best_mmr
                || (mmr_score == best_mmr
                    && best_idx.is_none_or(|bi: usize| results[candidate].1 > results[bi].1))
            {
                best_mmr = mmr_score;
                best_idx = Some(candidate);
            }
        }

        match best_idx {
            Some(idx) => {
                selected.push(idx);
                remaining.remove(&idx);
            }
            None => break,
        }
    }

    // Map back to original indices
    selected.iter().map(|&i| results[i].0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaccard_identical() {
        let sim = jaccard_similarity("hello world foo", "hello world foo");
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn jaccard_disjoint() {
        let sim = jaccard_similarity("hello world", "foo bar baz");
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn jaccard_partial_overlap() {
        // {hello, world} ∩ {hello, foo} = {hello}, union = {hello, world, foo}
        let sim = jaccard_similarity("hello world", "hello foo");
        assert!((sim - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn jaccard_empty_strings() {
        assert!((jaccard_similarity("", "") - 1.0).abs() < 1e-6);
        assert!(jaccard_similarity("hello", "").abs() < 1e-6);
        assert!(jaccard_similarity("", "hello").abs() < 1e-6);
    }

    #[test]
    fn jaccard_case_insensitive() {
        let sim = jaccard_similarity("Hello World", "hello world");
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mmr_empty_input() {
        let results: Vec<(usize, f32, &str)> = vec![];
        let reordered = mmr_rerank(&results, 0.7, 5);
        assert!(reordered.is_empty());
    }

    #[test]
    fn mmr_single_item() {
        let results = vec![(0, 0.9, "hello world")];
        let reordered = mmr_rerank(&results, 0.7, 5);
        assert_eq!(reordered, vec![0]);
    }

    #[test]
    fn mmr_lambda_one_preserves_order() {
        let results = vec![
            (0, 0.9, "rust programming language"),
            (1, 0.7, "python scripting"),
            (2, 0.5, "javascript web development"),
        ];
        let reordered = mmr_rerank(&results, 1.0, 3);
        assert_eq!(reordered, vec![0, 1, 2]);
    }

    #[test]
    fn mmr_promotes_diversity() {
        // Two near-duplicates and one unique result
        let results = vec![
            (0, 0.95, "the quick brown fox jumps over the lazy dog"),
            (1, 0.90, "the quick brown fox jumps over the lazy cat"),
            (2, 0.80, "rust programming language systems performance"),
        ];
        let reordered = mmr_rerank(&results, 0.5, 3);
        // With lambda=0.5, the unique result should be promoted above the duplicate
        assert_eq!(reordered[0], 0, "highest score should still be first");
        assert_eq!(
            reordered[1], 2,
            "diverse result should be promoted over near-duplicate"
        );
        assert_eq!(reordered[2], 1, "duplicate should be last");
    }

    #[test]
    fn mmr_respects_max_results() {
        let results = vec![(0, 0.9, "a"), (1, 0.8, "b"), (2, 0.7, "c"), (3, 0.6, "d")];
        let reordered = mmr_rerank(&results, 0.7, 2);
        assert_eq!(reordered.len(), 2);
    }

    #[test]
    fn mmr_all_identical_scores() {
        let results = vec![
            (0, 0.8, "alpha beta gamma"),
            (1, 0.8, "delta epsilon zeta"),
            (2, 0.8, "alpha beta gamma"),
        ];
        let reordered = mmr_rerank(&results, 0.7, 3);
        assert_eq!(reordered.len(), 3);
        // All should be included despite identical scores
    }

    #[test]
    fn mmr_lambda_zero_pure_diversity() {
        let results = vec![
            (0, 0.95, "the quick brown fox"),
            (1, 0.90, "the quick brown cat"),
            (2, 0.50, "rust programming language"),
        ];
        let reordered = mmr_rerank(&results, 0.0, 3);
        assert_eq!(reordered.len(), 3);
        // With lambda=0, after picking first, diversity should push the unique item up
    }

    #[test]
    fn mmr_large_result_set() {
        let results: Vec<(usize, f32, &str)> = (0..500)
            .map(|i| (i, 1.0 - (i as f32 / 500.0), "some text content for testing"))
            .collect();
        let reordered = mmr_rerank(&results, 0.7, 10);
        assert_eq!(reordered.len(), 10);
    }

    #[test]
    fn mmr_lambda_boundary_clamped() {
        let results = vec![(0, 0.9, "hello world"), (1, 0.5, "goodbye world")];
        // Lambda > 1.0 should clamp to 1.0 (pure relevance)
        let clamped_high = mmr_rerank(&results, 1.5, 2);
        let at_one = mmr_rerank(&results, 1.0, 2);
        assert_eq!(clamped_high, at_one, "lambda > 1.0 should clamp to 1.0");
        // Lambda < 0.0 should clamp to 0.0 (pure diversity)
        let clamped_low = mmr_rerank(&results, -0.5, 2);
        let at_zero = mmr_rerank(&results, 0.0, 2);
        assert_eq!(clamped_low, at_zero, "lambda < 0.0 should clamp to 0.0");
    }
}
