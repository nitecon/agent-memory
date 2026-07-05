pub mod bm25;
pub mod fusion;
pub mod rerank;
pub mod vector;

use rusqlite::Connection;
use std::path::Path;

use crate::db::models::Memory;
use crate::db::queries;
use crate::embedding;
use crate::error::MemoryError;

use self::bm25::search_bm25;
use self::fusion::{reciprocal_rank_fusion, RankedResult};
use self::vector::vector_search;

pub struct SearchResult {
    pub memory: Memory,
    pub rank_info: RankedResult,
    /// Classified match quality. Not currently surfaced in the light-XML
    /// output (Release 1 of the format change drops the per-hit metadata),
    /// but retained so downstream consumers — tests, future `--verbose`
    /// flags, Release 2's dream compactor — can reason about rank tier.
    #[allow(dead_code)]
    pub match_quality: MatchQuality,
    pub is_current_project: bool,
    /// True when the memory is tagged with the global-scope sentinel project
    /// (e.g. `__global__`). Global-scope memories receive a smaller score
    /// boost than current-project memories so universal preferences surface
    /// across every repo without out-ranking strong local context.
    pub is_global: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchQuality {
    High,
    Medium,
    Low,
}

impl MatchQuality {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchQuality::High => "high",
            MatchQuality::Medium => "medium",
            MatchQuality::Low => "low",
        }
    }
}

/// High: top-5 in both BM25 and vector lists.
/// Medium: top-5 in one list, or top-10 in both.
/// Low: anything else (deep tail of one list, absent from the other).
fn classify_quality(bm25: Option<usize>, vector: Option<usize>) -> MatchQuality {
    match (bm25, vector) {
        (Some(b), Some(v)) if b < 5 && v < 5 => MatchQuality::High,
        (Some(b), Some(v)) if b < 10 && v < 10 => MatchQuality::Medium,
        (Some(b), _) if b < 5 => MatchQuality::Medium,
        (_, Some(v)) if v < 5 => MatchQuality::Medium,
        _ => MatchQuality::Low,
    }
}

#[derive(Clone)]
pub struct SearchOptions<'a> {
    pub limit: usize,
    /// Project whose memories receive a score boost. Typically the cwd-derived
    /// project ident; `None` disables the current-project boost entirely.
    pub current_project: Option<&'a str>,
    /// Multiplier applied to current-project scores before re-sorting.
    pub boost_factor: f32,
    /// Hard filter: only return memories whose `project` equals this string.
    pub only_project: Option<&'a str>,
    /// Project ident that flags a memory as "global scope" (universal user
    /// preference that applies across every repo). Typically the sentinel
    /// string `__global__`. `None` disables the global boost entirely.
    pub global_project: Option<&'a str>,
    /// Multiplier applied to global-scope scores before re-sorting. Should be
    /// smaller than `boost_factor` so local context still wins ties, but
    /// larger than 1.0 so universal preferences out-rank cross-project noise.
    pub global_boost_factor: f32,
}

impl<'a> SearchOptions<'a> {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            current_project: None,
            boost_factor: 1.0,
            only_project: None,
            global_project: None,
            global_boost_factor: 1.0,
        }
    }
}

pub fn hybrid_search(
    conn: &Connection,
    query: &str,
    opts: SearchOptions<'_>,
    model_cache_dir: &Path,
) -> Result<Vec<SearchResult>, MemoryError> {
    // Over-fetch from both rankers so the post-fusion project boost has
    // headroom to reshuffle instead of working within a tiny top-K.
    let candidate_limit = opts.limit.saturating_mul(3).max(opts.limit);

    let bm25_results = search_bm25(conn, query, candidate_limit).unwrap_or_default();

    let query_embedding = embedding::embed_text(query, model_cache_dir)?;
    let all_embeddings = queries::get_all_embeddings(conn)?;
    let vector_results = vector_search(&query_embedding, &all_embeddings, candidate_limit);

    let fused = reciprocal_rank_fusion(&bm25_results, &vector_results, candidate_limit);

    let mut results = Vec::new();
    let mut accessed_ids = Vec::new();

    for ranked in fused {
        match queries::get_memory_by_id(conn, &ranked.id) {
            Ok(memory) => {
                let quality = classify_quality(ranked.bm25_rank, ranked.vector_rank);
                let is_current = match opts.current_project {
                    Some(cp) => memory.project.as_deref() == Some(cp),
                    None => false,
                };
                // A memory's project ident is a single string, so a memory
                // can be current-project or global-scope but never both.
                let is_global = match opts.global_project {
                    Some(gp) => memory.project.as_deref() == Some(gp),
                    None => false,
                };
                results.push(SearchResult {
                    memory,
                    rank_info: ranked,
                    match_quality: quality,
                    is_current_project: is_current,
                    is_global,
                });
            }
            Err(MemoryError::NotFound(_)) => continue,
            Err(e) => return Err(e),
        }
    }

    // Cross-encoder rerank of the full candidate set, in place. Runs after the
    // RRF-ordered `results` are materialized (so we have the document texts) and
    // before scope boosts (which multiply + re-sort). On success every result's
    // score is overwritten with the sigmoid-normalized rerank score for its
    // `content`; the final ordering is then driven by rerank score × scope
    // boost. Disabled-by-env or any rerank failure (offline, download error)
    // silently falls back to the existing RRF scores so `memory context` always
    // returns — the error is never propagated out of `hybrid_search`.
    maybe_rerank(query, &mut results, model_cache_dir);

    apply_scope_boosts(
        &mut results,
        opts.current_project.is_some().then_some(opts.boost_factor),
        opts.global_project
            .is_some()
            .then_some(opts.global_boost_factor),
    );

    if let Some(op) = opts.only_project {
        results.retain(|r| r.memory.project.as_deref() == Some(op));
    }

    results.truncate(opts.limit);

    for r in &results {
        accessed_ids.push(r.memory.id.clone());
    }
    queries::increment_access(conn, &accessed_ids)?;

    Ok(results)
}

/// Reranking is on by default. The `MEMORY_RERANK` env var is an escape hatch:
/// set it to `0`/`false`/`off`/`no`/`n` (case-insensitive) to disable. Any
/// other value — or an unset var — leaves reranking enabled. Mirrors the
/// boolean vocabulary `config::parse_bool` already accepts elsewhere.
fn rerank_enabled() -> bool {
    match std::env::var("MEMORY_RERANK") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no" | "n"
        ),
        Err(_) => true,
    }
}

/// Rerank `results` in place when enabled, overwriting each result's score with
/// the sigmoid-normalized cross-encoder score for its `memory.content`.
///
/// Graceful degradation is the whole point: if reranking is disabled or the
/// reranker fails for any reason, this is a no-op and the caller keeps the
/// RRF-ordered scores untouched. Errors are swallowed deliberately — recall must
/// not break because a model download failed on a freshly-synced host.
fn maybe_rerank(query: &str, results: &mut [SearchResult], model_cache_dir: &Path) {
    if results.is_empty() || !rerank_enabled() {
        return;
    }

    let docs: Vec<&str> = results.iter().map(|r| r.memory.content.as_str()).collect();
    match rerank::rerank_scores(query, &docs, model_cache_dir) {
        Ok(scores) if scores.len() == results.len() => {
            for (r, score) in results.iter_mut().zip(scores) {
                r.rank_info.score = score;
            }
        }
        // A mismatched length would corrupt the score/result pairing; treat it
        // like any other failure and fall back to the RRF ordering.
        Ok(_) | Err(_) => {}
    }
}

/// Apply per-scope score multipliers in place and re-sort by descending score.
///
/// Pulled out of `hybrid_search` so the boost-and-sort logic can be exercised
/// in unit tests without spinning up a database and embedding model. Each
/// multiplier is optional: `None` or a multiplier of exactly `1.0` means "no
/// boost for this scope" and short-circuits to avoid the float multiply.
/// When both multipliers are effectively disabled, the function is a no-op and
/// leaves the input ordering untouched.
///
/// Current-project takes precedence over global when both would match, but in
/// practice a memory's `project` column is a single string so `is_current_project`
/// and `is_global` are mutually exclusive. The `else if` is a belt-and-braces
/// guard for future schema changes.
fn apply_scope_boosts(
    results: &mut [SearchResult],
    current_project_factor: Option<f32>,
    global_factor: Option<f32>,
) {
    let cp_factor = current_project_factor.filter(|f| *f != 1.0);
    let g_factor = global_factor.filter(|f| *f != 1.0);
    if cp_factor.is_none() && g_factor.is_none() {
        return;
    }
    for r in results.iter_mut() {
        if r.is_current_project {
            if let Some(f) = cp_factor {
                r.rank_info.score *= f;
            }
        } else if r.is_global {
            if let Some(f) = g_factor {
                r.rank_info.score *= f;
            }
        }
    }
    results.sort_by(|a, b| {
        b.rank_info
            .score
            .partial_cmp(&a.rank_info.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Memory;
    use crate::search::fusion::RankedResult;

    /// Build a test-only `SearchResult` with a fixed RRF score and scope
    /// flags. Avoids constructing full embeddings — the boost logic only
    /// reads score + scope flags.
    fn mk_result(
        id: &str,
        project: Option<&str>,
        score: f32,
        cwd: &str,
        global: &str,
    ) -> SearchResult {
        let memory = Memory::new(
            format!("content-{id}"),
            None,
            project.map(String::from),
            None,
            None,
            Some("project".to_string()),
        );
        let is_current = memory.project.as_deref() == Some(cwd);
        let is_global = memory.project.as_deref() == Some(global);
        SearchResult {
            memory,
            rank_info: RankedResult {
                id: id.to_string(),
                score,
                bm25_rank: Some(0),
                vector_rank: Some(0),
            },
            match_quality: MatchQuality::High,
            is_current_project: is_current,
            is_global,
        }
    }

    /// Three memories tied on RRF score: current-project > global > other
    /// after applying 1.5× / 1.25× / 1.0× respectively. Order must match.
    #[test]
    fn apply_scope_boosts_orders_current_then_global_then_other() {
        let mut results = vec![
            mk_result(
                "other",
                Some("github.com/other/repo"),
                0.1,
                "agent-memory",
                "__global__",
            ),
            mk_result(
                "global",
                Some("__global__"),
                0.1,
                "agent-memory",
                "__global__",
            ),
            mk_result(
                "current",
                Some("agent-memory"),
                0.1,
                "agent-memory",
                "__global__",
            ),
        ];
        apply_scope_boosts(&mut results, Some(1.5), Some(1.25));
        assert_eq!(results[0].memory.project.as_deref(), Some("agent-memory"));
        assert_eq!(results[1].memory.project.as_deref(), Some("__global__"));
        assert_eq!(
            results[2].memory.project.as_deref(),
            Some("github.com/other/repo")
        );
    }

    /// A strong cross-project hit (base score well above the tied trio) still
    /// wins over a current-project boost when the pre-boost gap is large.
    #[test]
    fn strong_cross_project_can_still_out_rank_current_after_boost() {
        let mut results = vec![
            mk_result(
                "strong-other",
                Some("github.com/other/repo"),
                1.0,
                "agent-memory",
                "__global__",
            ),
            mk_result(
                "weak-current",
                Some("agent-memory"),
                0.5,
                "agent-memory",
                "__global__",
            ),
        ];
        apply_scope_boosts(&mut results, Some(1.5), Some(1.25));
        // weak-current boosted to 0.75; strong-other stays at 1.0 → other wins.
        assert_eq!(results[0].rank_info.id, "strong-other");
    }

    /// When both factors are 1.0 (or disabled), scoring + ordering is untouched.
    #[test]
    fn apply_scope_boosts_is_noop_when_factors_disabled() {
        let mut results = vec![
            mk_result("a", Some("agent-memory"), 0.3, "agent-memory", "__global__"),
            mk_result("b", Some("__global__"), 0.2, "agent-memory", "__global__"),
        ];
        let before: Vec<f32> = results.iter().map(|r| r.rank_info.score).collect();
        apply_scope_boosts(&mut results, None, None);
        let after: Vec<f32> = results.iter().map(|r| r.rank_info.score).collect();
        assert_eq!(before, after);

        apply_scope_boosts(&mut results, Some(1.0), Some(1.0));
        let after2: Vec<f32> = results.iter().map(|r| r.rank_info.score).collect();
        assert_eq!(before, after2);
    }

    /// With reranking disabled via `MEMORY_RERANK=0`, `maybe_rerank` is a no-op:
    /// scores and ordering are exactly the RRF input. Exercises the graceful
    /// fallback path without a DB or the reranker model. The env var is set and
    /// cleared within the test; `MEMORY_RERANK` is unique to rerank so this does
    /// not collide with other env-reading tests.
    #[test]
    fn maybe_rerank_is_noop_when_disabled_by_env() {
        std::env::set_var("MEMORY_RERANK", "0");

        let mut results = vec![
            mk_result("a", Some("agent-memory"), 0.9, "agent-memory", "__global__"),
            mk_result("b", Some("__global__"), 0.4, "agent-memory", "__global__"),
        ];
        let before_scores: Vec<f32> = results.iter().map(|r| r.rank_info.score).collect();
        let before_ids: Vec<String> = results.iter().map(|r| r.rank_info.id.clone()).collect();

        // model_cache_dir is never touched because the env disable short-circuits
        // before any model access.
        maybe_rerank("any query", &mut results, Path::new("/nonexistent"));

        let after_scores: Vec<f32> = results.iter().map(|r| r.rank_info.score).collect();
        let after_ids: Vec<String> = results.iter().map(|r| r.rank_info.id.clone()).collect();
        assert_eq!(before_scores, after_scores);
        assert_eq!(before_ids, after_ids);

        std::env::remove_var("MEMORY_RERANK");
    }

    /// The env disable predicate accepts the documented falsey vocabulary
    /// (case-insensitive) and treats unset / anything-else as enabled.
    #[test]
    fn rerank_enabled_respects_env_vocabulary() {
        for v in ["0", "false", "OFF", "No", "n", " false "] {
            std::env::set_var("MEMORY_RERANK", v);
            assert!(!rerank_enabled(), "expected {v:?} to disable rerank");
        }
        for v in ["1", "true", "on", "anything"] {
            std::env::set_var("MEMORY_RERANK", v);
            assert!(rerank_enabled(), "expected {v:?} to keep rerank enabled");
        }
        std::env::remove_var("MEMORY_RERANK");
        assert!(
            rerank_enabled(),
            "unset MEMORY_RERANK should keep rerank on"
        );
    }

    /// Global boost alone (no current-project boost) still elevates global
    /// memories over untagged/other-project ones — useful when the cwd can't
    /// be derived but the user still has universal preferences on file.
    #[test]
    fn apply_scope_boosts_global_only_elevates_global_memories() {
        let mut results = vec![
            mk_result(
                "other",
                Some("github.com/other/repo"),
                0.2,
                "agent-memory",
                "__global__",
            ),
            mk_result(
                "global",
                Some("__global__"),
                0.2,
                "agent-memory",
                "__global__",
            ),
        ];
        apply_scope_boosts(&mut results, None, Some(1.25));
        assert_eq!(results[0].memory.project.as_deref(), Some("__global__"));
    }
}
