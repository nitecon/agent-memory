pub mod bm25;
pub mod fusion;
pub mod index;
pub mod rerank;
pub mod vector;

use rusqlite::Connection;
use std::collections::{BTreeSet, HashSet};
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
    pub concept_type: String,
    pub concept_title: Option<String>,
    pub concept_description: Option<String>,
    pub concept_status: String,
    pub is_stale: bool,
    pub is_verified: bool,
    pub revision: i64,
    pub canonical_uri: String,
    pub graph_relation: Option<String>,
    pub graph_distance: Option<usize>,
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
    /// Exact OKF concept type filter.
    pub concept_type: Option<&'a str>,
    /// Exact tag filter.
    pub tag: Option<&'a str>,
    /// Text-seeded relationship expansion. Zero disables graph expansion.
    pub graph_depth: usize,
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
            concept_type: None,
            tag: None,
            graph_depth: 0,
        }
    }
}

struct ConceptSignals {
    concept_type: String,
    title: Option<String>,
    description: Option<String>,
    status: String,
    stale: bool,
    verified: bool,
    revision: i64,
}

fn concept_signals(conn: &Connection, id: &str) -> Result<ConceptSignals, MemoryError> {
    let (concept_type, title, description, status, stale_after, revision, verified) = conn.query_row(
        "SELECT c.concept_type, c.title, c.description, c.status, c.stale_after, c.current_revision,
                EXISTS(SELECT 1 FROM memory_verifications v WHERE v.memory_id = c.memory_id)
         FROM memory_concepts c WHERE c.memory_id = ?1",
        rusqlite::params![id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, bool>(6)?,
            ))
        },
    )?;
    let stale = stale_after
        .as_deref()
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .is_some_and(|at| at <= chrono::Utc::now());
    Ok(ConceptSignals {
        concept_type,
        title,
        description,
        status,
        stale,
        verified,
        revision,
    })
}

fn make_result(
    conn: &Connection,
    memory: Memory,
    ranked: RankedResult,
    opts: &SearchOptions<'_>,
    graph_relation: Option<String>,
    graph_distance: Option<usize>,
) -> Result<Option<SearchResult>, MemoryError> {
    // Enrich on use. A memory that is being surfaced is a memory worth
    // describing, and structural descriptors are cheap enough to derive
    // inline — so recall stops waiting on a scheduled Dream pass to gain
    // titles. Idempotent and best-effort: a locked or read-only store
    // degrades to the un-enriched projection read below.
    crate::concepts::enrich::enrich_quietly(conn, &memory.id);
    let signals = concept_signals(conn, &memory.id)?;
    // Single lifecycle predicate for every retrieval surface. `deprecated`
    // status and supersession both mean "this concept is no longer the live
    // answer", so neither may reach a ranked result — regardless of whether
    // the row arrived from the text rankers or from graph expansion.
    //
    // The graph path is the one that actually needed this: traversal is
    // `Direction::Both`, so a live memory's `supersedes` edge walks *backwards*
    // to the predecessor Dream already merged away. Without the filter that
    // predecessor is re-injected alongside the memory that replaced it — two
    // copies of the same knowledge, one of them stale — and `memory get` then
    // refuses the ID because prefix resolution hides superseded rows.
    if signals.status == "deprecated"
        || memory.superseded_by.is_some()
        || opts
            .concept_type
            .is_some_and(|expected| signals.concept_type != expected)
        || opts.tag.is_some_and(|expected| {
            !memory
                .tags
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|tag| tag == expected)
        })
        || opts
            .only_project
            .is_some_and(|project| memory.project.as_deref() != Some(project))
    {
        return Ok(None);
    }
    let is_current_project = opts
        .current_project
        .is_some_and(|project| memory.project.as_deref() == Some(project));
    let is_global = opts
        .global_project
        .is_some_and(|project| memory.project.as_deref() == Some(project));
    let canonical_uri = crate::concepts::canonical_uri(&memory.id, memory.project.as_deref());
    Ok(Some(SearchResult {
        memory,
        match_quality: classify_quality(ranked.bm25_rank, ranked.vector_rank),
        rank_info: ranked,
        is_current_project,
        is_global,
        concept_type: signals.concept_type,
        concept_title: signals.title,
        concept_description: signals.description,
        concept_status: signals.status,
        is_stale: signals.stale,
        is_verified: signals.verified,
        revision: signals.revision,
        canonical_uri,
        graph_relation,
        graph_distance,
    }))
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

    for ranked in fused {
        match queries::get_memory_by_id(conn, &ranked.id) {
            Ok(memory) => {
                if let Some(result) = make_result(conn, memory, ranked, &opts, None, None)? {
                    results.push(result);
                }
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

    if opts.graph_depth > 0 && !results.is_empty() {
        // Graph projection is advisory. Corrupt/missing derived graph state
        // must fall back to the already-ranked flat textual results.
        let _ = expand_graph_neighbors(conn, &mut results, &opts);
    }

    apply_trust_modifiers(&mut results);

    apply_scope_boosts(
        &mut results,
        opts.current_project.is_some().then_some(opts.boost_factor),
        opts.global_project
            .is_some()
            .then_some(opts.global_boost_factor),
    );

    results.truncate(opts.limit);

    increment_surfaced_access(conn, &results)?;

    Ok(results)
}

fn increment_surfaced_access(
    conn: &Connection,
    results: &[SearchResult],
) -> Result<(), MemoryError> {
    let mut accessed_ids = results
        .iter()
        .map(|result| result.memory.id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    accessed_ids.sort();
    queries::increment_access(conn, &accessed_ids)
}

fn apply_trust_modifiers(results: &mut [SearchResult]) {
    for result in results {
        if result.is_stale {
            result.rank_info.score *= 0.9;
        }
    }
}

fn relation_factor(relation: &str) -> f32 {
    match relation {
        "supports" => 0.80,
        "supersedes" => 0.78,
        "derived_from" => 0.70,
        "links_to" => 0.65,
        "cites" => 0.62,
        _ => 0.60,
    }
}

fn expand_graph_neighbors(
    conn: &Connection,
    results: &mut Vec<SearchResult>,
    opts: &SearchOptions<'_>,
) -> Result<(), MemoryError> {
    use crate::concepts::graph::{self, Direction, TraversalOptions};

    let seed_limit = results.len().min(opts.limit.max(1));
    let seeds = results
        .iter()
        .take(seed_limit)
        .map(|result| (result.memory.id.clone(), result.rank_info.score))
        .collect::<Vec<_>>();
    let seed_scores = seeds
        .iter()
        .cloned()
        .collect::<std::collections::HashMap<_, _>>();
    let roots = seeds.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
    let traversal = graph::traverse(
        conn,
        &roots,
        &TraversalOptions {
            direction: Direction::Both,
            relations: BTreeSet::new(),
            max_depth: opts.graph_depth.min(2),
            fan_out: 12,
            max_results: opts.limit.saturating_mul(3).clamp(1, 300),
        },
    )?;
    let mut known = results
        .iter()
        .map(|result| result.memory.id.clone())
        .collect::<HashSet<_>>();
    let neighbor_budget = opts.limit.div_ceil(3).max(1);
    let mut added = 0;
    for path in traversal.paths {
        if added >= neighbor_budget {
            break;
        }
        let Some(step) = path.steps.last() else {
            continue;
        };
        let Some(seed) = path.nodes.first() else {
            continue;
        };
        let Some(neighbor) = path.nodes.last() else {
            continue;
        };
        if known.contains(neighbor) || path.cycle {
            continue;
        }
        let distance = path.steps.len();
        let decay = 0.75_f32.powi(distance.saturating_sub(1) as i32);
        let score = seed_scores.get(seed).copied().unwrap_or_default()
            * relation_factor(&step.relation)
            * decay;
        let memory = match queries::get_memory_by_id(conn, neighbor) {
            Ok(memory) => memory,
            Err(MemoryError::NotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        let ranked = RankedResult {
            id: neighbor.clone(),
            score,
            bm25_rank: None,
            vector_rank: None,
        };
        if let Some(result) = make_result(
            conn,
            memory,
            ranked,
            opts,
            Some(step.relation.clone()),
            Some(distance),
        )? {
            known.insert(neighbor.clone());
            results.push(result);
            added += 1;
        }
    }
    Ok(())
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

    let documents = results
        .iter()
        .map(|result| {
            let tags = result.memory.tags.as_deref().unwrap_or_default().join(" ");
            format!(
                "{}\n{}\n{}\n{}\n{}",
                result.concept_title.as_deref().unwrap_or_default(),
                result.concept_description.as_deref().unwrap_or_default(),
                result.concept_type,
                tags,
                result.memory.content
            )
        })
        .collect::<Vec<_>>();
    let docs = documents.iter().map(String::as_str).collect::<Vec<_>>();
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
    use crate::{concepts, db, db::queries};

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
            concept_type: "Agent Memory/project".to_string(),
            concept_title: None,
            concept_description: None,
            concept_status: "stable".to_string(),
            is_stale: false,
            is_verified: false,
            revision: 1,
            canonical_uri: format!("memory://unscoped/{id}"),
            graph_relation: None,
            graph_distance: None,
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

    #[test]
    fn okf_segment_index_searches_metadata_dedupes_resources_and_excludes_deprecated() {
        let conn = db::open_database(Path::new(":memory:")).unwrap();
        let memory = Memory::new(
            format!(
                "# Alpha\n{}\n## Beta\n{}",
                "repeated ".repeat(300),
                "needle ".repeat(300)
            ),
            Some(vec!["unique-tag".into()]),
            Some("project".into()),
            None,
            None,
            Some("reference".into()),
        );
        let id = memory.id.clone();
        queries::insert_memory(&conn, &memory).unwrap();
        concepts::mutate(&conn, &id, "metadata", None, None, true, |_| {
            conn.execute(
                "UPDATE memory_concepts SET title = 'Unique Lantern',
                 description = 'Special description' WHERE memory_id = ?1",
                rusqlite::params![id],
            )?;
            Ok(())
        })
        .unwrap();

        for query in [
            "lantern",
            "description",
            "unique-tag",
            "reference",
            "needle",
        ] {
            let hits = search_bm25(&conn, query, 10).unwrap();
            assert_eq!(
                hits.iter().filter(|(hit, _)| hit == &id).count(),
                1,
                "{query}"
            );
        }

        concepts::mutate(&conn, &id, "deprecate", None, None, true, |_| {
            conn.execute(
                "UPDATE memory_concepts SET status = 'deprecated' WHERE memory_id = ?1",
                rusqlite::params![id],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(search_bm25(&conn, "lantern", 10).unwrap().is_empty());
    }

    #[test]
    fn graph_expansion_is_text_seeded_bounded_filtered_and_stale_is_downranked() {
        let conn = db::open_database(Path::new(":memory:")).unwrap();
        let seed = Memory::new(
            "seed".into(),
            Some(vec!["keep".into()]),
            Some("p".into()),
            None,
            None,
            None,
        );
        let neighbor = Memory::new(
            "neighbor".into(),
            Some(vec!["keep".into()]),
            Some("p".into()),
            None,
            None,
            None,
        );
        let excluded = Memory::new(
            "excluded".into(),
            Some(vec!["other".into()]),
            Some("p".into()),
            None,
            None,
            None,
        );
        for memory in [&seed, &neighbor, &excluded] {
            queries::insert_memory(&conn, memory).unwrap();
        }
        concepts::insert_relationship(
            &conn,
            &seed.id,
            Some(&neighbor.id),
            &neighbor.id,
            "supports",
            1,
            None,
        )
        .unwrap();
        concepts::insert_relationship(
            &conn,
            &seed.id,
            Some(&excluded.id),
            &excluded.id,
            "supports",
            1,
            None,
        )
        .unwrap();
        let opts = SearchOptions {
            limit: 3,
            tag: Some("keep"),
            graph_depth: 1,
            ..SearchOptions::new(3)
        };
        let ranked = RankedResult {
            id: seed.id.clone(),
            score: 1.0,
            bm25_rank: Some(0),
            vector_rank: Some(0),
        };
        let mut results = vec![make_result(&conn, seed.clone(), ranked, &opts, None, None)
            .unwrap()
            .unwrap()];
        expand_graph_neighbors(&conn, &mut results, &opts).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].memory.id, neighbor.id);
        assert_eq!(results[1].graph_relation.as_deref(), Some("supports"));
        assert_eq!(results[1].graph_distance, Some(1));

        results[0].is_stale = true;
        let score = results[0].rank_info.score;
        apply_trust_modifiers(&mut results);
        assert!((results[0].rank_info.score - score * 0.9).abs() < f32::EPSILON);
    }

    /// Regression: a live memory's `supersedes` edge points backwards at the
    /// predecessor Dream merged away. `Direction::Both` traversal reaches it,
    /// so without the lifecycle predicate recall injects the survivor and the
    /// row it replaced side by side.
    #[test]
    fn graph_expansion_never_resurfaces_the_superseded_predecessor() {
        let conn = db::open_database(Path::new(":memory:")).unwrap();
        let survivor = Memory::new(
            "survivor".into(),
            Some(vec!["keep".into()]),
            Some("p".into()),
            None,
            None,
            None,
        );
        let predecessor = Memory::new(
            "predecessor".into(),
            Some(vec!["keep".into()]),
            Some("p".into()),
            None,
            None,
            None,
        );
        for memory in [&survivor, &predecessor] {
            queries::insert_memory(&conn, memory).unwrap();
        }
        queries::mark_superseded(&conn, &predecessor.id, &survivor.id).unwrap();

        let opts = SearchOptions {
            limit: 5,
            graph_depth: 1,
            ..SearchOptions::new(5)
        };
        let ranked = RankedResult {
            id: survivor.id.clone(),
            score: 1.0,
            bm25_rank: Some(0),
            vector_rank: Some(0),
        };
        let mut results = vec![
            make_result(&conn, survivor.clone(), ranked, &opts, None, None)
                .unwrap()
                .unwrap(),
        ];
        expand_graph_neighbors(&conn, &mut results, &opts).unwrap();
        assert_eq!(results.len(), 1, "superseded predecessor must not be added");

        // The predecessor is also rejected when it arrives from the text
        // rankers rather than from graph expansion.
        let direct = RankedResult {
            id: predecessor.id.clone(),
            score: 1.0,
            bm25_rank: Some(0),
            vector_rank: Some(0),
        };
        let refreshed = queries::get_memory_by_id(&conn, &predecessor.id).unwrap();
        assert!(make_result(&conn, refreshed, direct, &opts, None, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn surfaced_resource_access_is_incremented_once_even_if_internal_results_repeat() {
        let conn = db::open_database(Path::new(":memory:")).unwrap();
        let memory = Memory::new("body".into(), None, None, None, None, None);
        let id = memory.id.clone();
        queries::insert_memory(&conn, &memory).unwrap();
        let first = mk_result("one", None, 1.0, "p", "__global__");
        let mut second = mk_result("two", None, 0.5, "p", "__global__");
        let mut first = first;
        first.memory.id = id.clone();
        second.memory.id = id.clone();
        increment_surfaced_access(&conn, &[first, second]).unwrap();
        assert_eq!(
            queries::get_memory_by_id(&conn, &id).unwrap().access_count,
            1
        );
    }
}
