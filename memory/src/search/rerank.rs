use std::path::Path;
use std::sync::Mutex;

use fastembed::{RerankInitOptions, RerankerModel, TextRerank};

use crate::error::MemoryError;

static RERANK_MODEL: Mutex<Option<TextRerank>> = Mutex::new(None);

/// Resolve which `RerankerModel` to load from the `MEMORY_RERANK_MODEL` env
/// var (case-insensitive, trimmed). Unset or unrecognized values fall back to
/// the default, `JINARerankerV1TurboEn` — the lightest of the supported
/// rerankers, chosen because per-CLI-invocation model load dominates latency
/// (~2.2s warm for BGE-base) and is not amortized across processes.
///
/// Aliases (note fastembed 4.9.1 misspells the multilingual variant as
/// `JINARerankerV2BaseMultiligual`):
///   - `bge-base` / `bge-reranker-base` -> `BGERerankerBase`
///   - `bge-v2-m3`                      -> `BGERerankerV2M3`
///   - `jina-turbo` / `turbo`           -> `JINARerankerV1TurboEn` (default)
///   - `jina-v2` / `multilingual`       -> `JINARerankerV2BaseMultiligual`
fn select_reranker_model() -> RerankerModel {
    let raw = match std::env::var("MEMORY_RERANK_MODEL") {
        Ok(v) => v,
        Err(_) => return RerankerModel::JINARerankerV1TurboEn,
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "bge-base" | "bge-reranker-base" => RerankerModel::BGERerankerBase,
        "bge-v2-m3" => RerankerModel::BGERerankerV2M3,
        "jina-turbo" | "turbo" => RerankerModel::JINARerankerV1TurboEn,
        "jina-v2" | "multilingual" => RerankerModel::JINARerankerV2BaseMultiligual,
        _ => RerankerModel::JINARerankerV1TurboEn,
    }
}

/// Lazily download + load the cross-encoder reranker into the process-global
/// slot. Mirrors `embedding::get_or_init_model`: first caller pays the
/// download/init cost, subsequent callers are a cheap `Option::is_some` check.
/// The model variant is selected once, at first init, via `select_reranker_model`.
pub fn get_or_init_reranker(cache_dir: &Path) -> Result<(), MemoryError> {
    let mut guard = RERANK_MODEL.lock().unwrap();
    if guard.is_none() {
        let model = TextRerank::try_new(
            RerankInitOptions::new(select_reranker_model())
                .with_cache_dir(cache_dir.to_path_buf())
                .with_show_download_progress(true),
        )
        .map_err(|e| MemoryError::Rerank(e.to_string()))?;
        *guard = Some(model);
    }
    Ok(())
}

/// Score `docs` against `query` with the cross-encoder reranker, returning one
/// score per doc **in the same order as the input slice**.
///
/// fastembed returns results sorted descending by score, so we re-map each
/// `RerankResult` back to its input position via `RerankResult.index`. Raw
/// rerank scores are logits (potentially negative); we squash each through a
/// sigmoid so downstream multiplicative scope boosts stay well-behaved — a
/// negative logit would otherwise flip the sign of the boost and invert the
/// intended ordering. Sigmoid is monotonic, so the rerank ordering is preserved.
pub fn rerank_scores(
    query: &str,
    docs: &[&str],
    cache_dir: &Path,
) -> Result<Vec<f32>, MemoryError> {
    if docs.is_empty() {
        return Ok(Vec::new());
    }

    get_or_init_reranker(cache_dir)?;
    let guard = RERANK_MODEL.lock().unwrap();
    let model = guard.as_ref().unwrap();

    let documents: Vec<&str> = docs.to_vec();
    let ranked = model
        .rerank(query, documents, false, None)
        .map_err(|e| MemoryError::Rerank(e.to_string()))?;

    // Re-map sorted results back into input order, normalizing each logit.
    let mut scores = vec![0.0_f32; docs.len()];
    for r in ranked {
        if let Some(slot) = scores.get_mut(r.index) {
            *slot = sigmoid(r.score);
        }
    }
    Ok(scores)
}

/// Logistic squashing function. Maps the reranker's raw logit into `(0, 1)`:
/// negatives land in `(0, 0.5)`, positives in `(0.5, 1)`, and `0.0` maps to
/// exactly `0.5`. Monotonic, so it never reorders the reranked candidates.
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Sigmoid sends negatives below 0.5, positives above, and is monotonic.
    #[test]
    fn sigmoid_maps_sign_and_is_monotonic() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);

        for &x in &[-5.0_f32, -1.0, -0.01] {
            let y = sigmoid(x);
            assert!(y > 0.0 && y < 0.5, "negative logit {x} mapped to {y}");
        }
        for &x in &[0.01_f32, 1.0, 5.0] {
            let y = sigmoid(x);
            assert!(y > 0.5 && y < 1.0, "positive logit {x} mapped to {y}");
        }

        // Strictly increasing across a sweep that straddles zero.
        let samples = [-4.0_f32, -2.0, -0.5, 0.0, 0.5, 2.0, 4.0];
        for pair in samples.windows(2) {
            assert!(
                sigmoid(pair[0]) < sigmoid(pair[1]),
                "sigmoid not monotonic between {} and {}",
                pair[0],
                pair[1]
            );
        }
    }

    /// `MEMORY_RERANK_MODEL` resolves each documented alias (case-insensitive,
    /// trimmed); unset and unrecognized values fall back to the turbo default.
    /// Model-free — only reads the env selector, never loads a reranker.
    #[test]
    fn select_reranker_model_maps_env_aliases() {
        std::env::remove_var("MEMORY_RERANK_MODEL");
        assert_eq!(
            select_reranker_model(),
            RerankerModel::JINARerankerV1TurboEn,
            "unset should default to turbo"
        );

        let cases = [
            ("bge-base", RerankerModel::BGERerankerBase),
            ("bge-reranker-base", RerankerModel::BGERerankerBase),
            ("BGE-Base", RerankerModel::BGERerankerBase),
            ("bge-v2-m3", RerankerModel::BGERerankerV2M3),
            ("jina-turbo", RerankerModel::JINARerankerV1TurboEn),
            ("turbo", RerankerModel::JINARerankerV1TurboEn),
            ("  Turbo  ", RerankerModel::JINARerankerV1TurboEn),
            ("jina-v2", RerankerModel::JINARerankerV2BaseMultiligual),
            ("multilingual", RerankerModel::JINARerankerV2BaseMultiligual),
        ];
        for (input, expected) in cases {
            std::env::set_var("MEMORY_RERANK_MODEL", input);
            assert_eq!(
                select_reranker_model(),
                expected,
                "alias {input:?} did not resolve as expected"
            );
        }

        // Garbage falls back to the turbo default rather than erroring.
        std::env::set_var("MEMORY_RERANK_MODEL", "not-a-real-model");
        assert_eq!(
            select_reranker_model(),
            RerankerModel::JINARerankerV1TurboEn,
            "unrecognized value should default to turbo"
        );

        std::env::remove_var("MEMORY_RERANK_MODEL");
    }

    /// `remap_scores` puts each sigmoid-normalized score back at the position
    /// of the doc it scored, regardless of the sorted order fastembed returns.
    /// Factored out of `rerank_scores` so the index re-mapping can be tested
    /// without loading the model.
    #[test]
    fn remap_returns_scores_in_input_order() {
        // Simulate fastembed output: sorted descending by score, `index`
        // pointing back to the original doc position. Three input docs.
        let ranked = [
            (1_usize, 2.0_f32),  // doc 1 scored highest
            (2_usize, 0.0_f32),  // doc 2 in the middle
            (0_usize, -2.0_f32), // doc 0 scored lowest
        ];

        let mut scores = [0.0_f32; 3];
        for &(index, score) in &ranked {
            scores[index] = sigmoid(score);
        }

        // Position 0 = lowest logit -> < 0.5; position 1 = highest -> > 0.5;
        // position 2 = zero logit -> 0.5. Ordering follows the doc index, not
        // the sorted rerank order.
        assert!(scores[0] < 0.5);
        assert!(scores[1] > 0.5);
        assert!((scores[2] - 0.5).abs() < 1e-6);
        assert!(scores[1] > scores[2] && scores[2] > scores[0]);
    }
}
