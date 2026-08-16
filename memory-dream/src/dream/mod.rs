//! Dream orchestrator (v1.6.0).
//!
//! Entry point: [`run`]. Called once per `memory-dream` invocation.
//! Per-project pipeline:
//!
//! 1. **Enumerate projects** with at least one live memory.
//! 2. **Stage 0 — project review** ([`project_review::run_project`]).
//!    Primary cross-memory consolidation path. The model sees every
//!    memory in a project (or every memory in a clustered batch when
//!    the project is too large) and emits per-memory
//!    `keep`/`drop`/`merge_into`/`supersede_by`/`extract` decisions.
//!    Catches paraphrased duplicates that Stage A misses because they
//!    share no vocabulary.
//! 3. **Stage A — cosine dedup** ([`dedup::find_duplicate`] + policy).
//!    Secondary signal. Kept in place to catch byte-identical inserts
//!    that slipped through without a model round-trip. See
//!    [`project_review`] module docs for the rationale.
//! 4. **Stage B — per-memory condense**. For every remaining candidate we
//!    invoke the configured inference backend with the strict three-way
//!    prompt contract ([`condense::run_per_memory`]):
//!      * `skip` → no change.
//!      * `forget` → delete via the DB layer (not a `memory forget` shell).
//!      * otherwise → treat as a rewritten body, persist via
//!        `update_content` so `content_raw` preserves provenance.
//! 5. **Stamp** `project_state.last_dream_at` per project (Apply mode).
//!
//! Progress is emitted as light-XML on stdout — same renderer the rest of
//! the project uses — so a caller can pipe `memory-dream` output into a
//! log collector alongside `memory` command output.

pub mod condense;
pub mod dedup;
pub mod project_review;
pub mod prompt;

use std::path::Path;

use agent_memory::config::GatewayConfig;
use agent_memory::db::models::Memory;
use agent_memory::db::queries as q;
use agent_memory::embedding::embed_text;
use agent_memory::error::MemoryError;
use agent_memory::render;
use rusqlite::Connection;
use std::collections::HashSet;
use thiserror::Error;

use crate::inference::Inference;

/// Top-level errors for the dream orchestrator.
#[derive(Debug, Error)]
pub enum DreamError {
    #[error("db error: {0}")]
    Db(#[from] MemoryError),

    #[error("sqlite transaction error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Run mode for a dream pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamMode {
    /// Walk candidate memories and report intended decisions without
    /// writing. Stage A still classifies duplicates (no mutation); Stage B
    /// invokes the LLM but the parsed decision is discarded.
    Dry,
    /// Commit changes.
    Apply,
}

/// Short name used for the embedding model column. Mirrors the fastembed
/// default in `agent_memory::embedding::embed_text`.
pub const EMBEDDING_MODEL_NAME: &str = "all-MiniLM-L6-v2";

/// OKF `generated.by` for bodies this crate authored.
///
/// Carries the backend-qualified model identity, not just the producer, so a
/// curated body can be traced to the exact inference that wrote it — the same
/// identity discipline `condenser_version` already follows.
pub fn generated_by(model_name: &str) -> String {
    format!("memory-dream/{model_name}")
}

/// Configuration for a single dream pass.
pub struct DreamConfig<'a> {
    /// Which execution mode to run in.
    pub mode: DreamMode,
    /// Cap on the number of memories to process. `0` = no limit.
    pub limit: usize,
    /// Backend-qualified inference identity to stamp into `condenser_version`.
    pub model_name: &'a str,
    /// Cache directory for fastembed's MiniLM model files.
    pub embedding_cache_dir: &'a Path,
    /// Cosine threshold for Stage A dedup.
    pub cosine_threshold: f32,
    /// Force a full walk, ignoring both the
    /// `project_state.last_dream_at` time cutoff AND the
    /// `condenser_version` freshness check, so every live memory in every
    /// project flows into Stages 0 / A / B regardless of when it was last
    /// processed.
    ///
    /// Set via `memory-dream --full` (historical name) OR
    /// `memory-dream --refresh` (user-facing alias). `main.rs` folds both
    /// CLI flags into this single switch so every orchestrator site only
    /// needs one code path.
    pub full: bool,
    /// Reserved — per-memory condense doesn't batch, but the CLI flag is
    /// retained so existing automation keeps parsing. Ignored by the
    /// v1.5.0 orchestrator.
    pub batch_size_override: usize,
    /// Optional gateway configuration for direct update/tombstone writes.
    /// Tests default this off; the production CLI passes the loaded memory
    /// config so dream mutations stay in sync with the gateway when enabled.
    pub gateway_config: Option<&'a GatewayConfig>,
}

impl<'a> DreamConfig<'a> {
    /// Build a config with sensible defaults. The caller is still expected
    /// to fill in `mode` and `embedding_cache_dir`.
    pub fn new(mode: DreamMode, model_name: &'a str, embedding_cache_dir: &'a Path) -> Self {
        Self {
            mode,
            limit: 0,
            model_name,
            embedding_cache_dir,
            cosine_threshold: dedup::DEFAULT_COSINE_THRESHOLD,
            full: false,
            batch_size_override: 0,
            gateway_config: None,
        }
    }
}

/// Summary of a completed dream pass. Returned to the CLI layer so the
/// binary can emit a single `<result .../>` line at the end.
///
/// Counters split across the three stages:
///   - Stage 0 (project review): `review_*` fields tally per-project
///     LLM decisions.
///   - Stage A (cosine dedup): `superseded`.
///   - Stage B (per-memory condense): `kept`, `rewritten`, `forgot`.
///   - `failed` is shared across stages.
#[derive(Debug, Default)]
pub struct DreamSummary {
    pub total_walked: usize,
    pub kept: usize,
    pub rewritten: usize,
    pub forgot: usize,
    pub superseded: usize,
    pub failed: usize,
    // -- Stage 0 (project review) counters ---------------------------------
    pub review_kept: usize,
    pub review_dropped: usize,
    pub review_merged: usize,
    pub review_superseded: usize,
    pub review_extracted: usize,
    pub review_contradictions: usize,
}

/// Run a full dream pass.
pub fn run(
    conn: &mut Connection,
    inference: &dyn Inference,
    cfg: &DreamConfig<'_>,
) -> Result<DreamSummary, DreamError> {
    let started_at = std::time::Instant::now();
    let mut summary = DreamSummary::default();
    let mut remaining = cfg.limit;

    // Enumerate projects. An empty DB yields zero projects and the pass
    // exits cleanly with zero work.
    let projects = q::list_distinct_projects_for_dream(conn)?;

    for project in &projects {
        if cfg.limit > 0 && remaining == 0 {
            break;
        }
        let project_label = project.as_deref().unwrap_or("(null)");
        let cutoff = resolve_incremental_cutoff(conn, project.as_deref(), cfg)?;
        let current_stamp = prompt::condenser_version_stamp(cfg.model_name);

        let candidates = q::list_dream_candidates(
            conn,
            project.as_deref(),
            cutoff.as_deref(),
            if cfg.full {
                None
            } else {
                Some(current_stamp.as_str())
            },
            if cfg.limit > 0 { remaining } else { 0 },
        )?;

        let walked = candidates.len();
        summary.total_walked += walked;
        if cfg.limit > 0 {
            remaining = remaining.saturating_sub(walked);
        }

        println!(
            "{}",
            render::render_action_result(
                "dream_project",
                &[
                    ("project", project_label.to_string()),
                    ("candidates", walked.to_string()),
                ]
            )
        );

        if walked == 0 {
            continue;
        }

        let mut project_stats = ProjectStats::default();

        // Stage 0 — project-level cross-memory review. Primary
        // consolidation path. Survivors (including newly-materialized
        // supersede/extract memories) flow into Stage A + B for the
        // existing cosine + per-memory polish work.
        let stage0 = run_stage_0_project_review(
            conn,
            inference,
            cfg,
            project.as_deref(),
            candidates,
            &mut project_stats,
            &mut summary,
        );

        // Stage A — cosine dedup over the Stage 0 survivors for this project.
        // Survivors move to Stage B.
        let survivors = run_stage_a_dedup(
            conn,
            cfg,
            &stage0.survivors,
            &stage0.contradiction_ids,
            &mut project_stats,
        );

        // Stage B — per-memory condense via the three-way contract.
        for mem in &survivors {
            run_stage_b_condense(conn, inference, cfg, mem, &mut project_stats);
        }

        summary.kept += project_stats.kept;
        summary.rewritten += project_stats.rewritten;
        summary.forgot += project_stats.forgot;
        summary.superseded += project_stats.superseded;
        summary.failed += project_stats.failed;

        println!(
            "{}",
            render::render_action_result(
                "dream_project_complete",
                &[
                    ("project", project_label.to_string()),
                    ("kept", project_stats.kept.to_string()),
                    ("rewritten", project_stats.rewritten.to_string()),
                    ("forgot", project_stats.forgot.to_string()),
                    ("superseded", project_stats.superseded.to_string()),
                    ("review_kept", project_stats.review_kept.to_string()),
                    ("review_dropped", project_stats.review_dropped.to_string()),
                    ("review_merged", project_stats.review_merged.to_string()),
                    (
                        "review_superseded",
                        project_stats.review_superseded.to_string()
                    ),
                    (
                        "review_extracted",
                        project_stats.review_extracted.to_string()
                    ),
                    (
                        "review_contradictions",
                        project_stats.review_contradictions.to_string()
                    ),
                    ("failed", project_stats.failed.to_string()),
                ]
            )
        );

        // Stamp the project's last_dream_at on Apply mode only — Dry runs
        // must not influence the next incremental pass.
        if cfg.mode == DreamMode::Apply {
            let now = chrono::Utc::now().to_rfc3339();
            if let Err(e) = q::set_last_dream_at(conn, project.as_deref(), &now) {
                tracing::warn!(project = %project_label, error = %e,
                    "failed to stamp project_state.last_dream_at");
            }
        }
    }

    println!(
        "{}",
        render::render_action_result(
            "dream_complete",
            &[
                ("walked", summary.total_walked.to_string()),
                ("kept", summary.kept.to_string()),
                ("rewritten", summary.rewritten.to_string()),
                ("forgot", summary.forgot.to_string()),
                ("superseded", summary.superseded.to_string()),
                ("review_kept", summary.review_kept.to_string()),
                ("review_dropped", summary.review_dropped.to_string()),
                ("review_merged", summary.review_merged.to_string()),
                ("review_superseded", summary.review_superseded.to_string()),
                ("review_extracted", summary.review_extracted.to_string()),
                (
                    "review_contradictions",
                    summary.review_contradictions.to_string()
                ),
                ("failed", summary.failed.to_string()),
                ("inference", cfg.model_name.to_string()),
                ("elapsed_ms", started_at.elapsed().as_millis().to_string()),
            ]
        )
    );

    Ok(summary)
}

/// Per-project tallies accumulated across Stages 0+A+B. Rolled up into
/// [`DreamSummary`] at the end of each project's iteration.
///
/// Stage 0 (project review) fills the `review_*` fields; Stage A fills
/// `superseded`; Stage B fills `kept`, `rewritten`, `forgot`. `failed`
/// is shared across stages — any transaction-rollback or parse-error
/// hit lands there.
#[derive(Debug, Default, Clone, Copy)]
struct ProjectStats {
    kept: usize,
    rewritten: usize,
    forgot: usize,
    superseded: usize,
    failed: usize,
    // Stage 0 tallies.
    review_kept: usize,
    review_dropped: usize,
    review_merged: usize,
    review_superseded: usize,
    review_extracted: usize,
    review_contradictions: usize,
}

struct Stage0Outcome {
    survivors: Vec<Memory>,
    contradiction_ids: HashSet<String>,
}

/// Decide which `last_dream_at` cutoff applies for `project`.
///
/// Three cases, collapsed into one return value:
///   - `cfg.full == true`              → `None` (no cutoff; re-walk all).
///   - No prior pass (NULL row)        → `None` (first pass processes everything).
///   - Prior pass timestamp present    → `Some(ts)`.
fn resolve_incremental_cutoff(
    conn: &Connection,
    project: Option<&str>,
    cfg: &DreamConfig<'_>,
) -> Result<Option<String>, DreamError> {
    if cfg.full {
        return Ok(None);
    }
    Ok(q::get_last_dream_at(conn, project)?)
}

/// Stage 0 — project-level cross-memory review.
///
/// Sends the project's full candidate set (or clustered batches for
/// oversized projects) to the model, applies the returned per-memory
/// decisions, and returns the survivors. Survivors include:
///   - Memories the model flagged `keep`.
///   - Newly-materialized memories from `supersede_by` and `extract`
///     decisions (the original rows are deleted in Apply mode).
///
/// Dropped and merged memories are not returned as survivors — they're
/// gone from the pipeline after Stage 0.
///
/// On inference failure the pass degrades gracefully: all candidates
/// pass through as survivors, failures are counted, and Stages A + B
/// still run. This matches the existing `NoopInference` fallback used
/// when no model is available.
fn run_stage_0_project_review(
    conn: &mut rusqlite::Connection,
    inference: &dyn Inference,
    cfg: &DreamConfig<'_>,
    project: Option<&str>,
    candidates: Vec<Memory>,
    project_stats: &mut ProjectStats,
    summary: &mut DreamSummary,
) -> Stage0Outcome {
    let apply = cfg.mode == DreamMode::Apply;
    let project_label = project.unwrap_or("(null)");

    match project_review::run_project(
        conn,
        inference,
        project,
        candidates.clone(),
        cfg.model_name,
        cfg.embedding_cache_dir,
        cfg.gateway_config,
        apply,
    ) {
        Ok(outcome) => {
            project_stats.review_kept += outcome.stats.kept;
            project_stats.review_dropped += outcome.stats.dropped;
            project_stats.review_merged += outcome.stats.merged;
            project_stats.review_superseded += outcome.stats.superseded;
            project_stats.review_extracted += outcome.stats.extracted;
            project_stats.review_contradictions += outcome.stats.contradictions;
            project_stats.failed += outcome.stats.failed;

            summary.review_kept += outcome.stats.kept;
            summary.review_dropped += outcome.stats.dropped;
            summary.review_merged += outcome.stats.merged;
            summary.review_superseded += outcome.stats.superseded;
            summary.review_extracted += outcome.stats.extracted;
            summary.review_contradictions += outcome.stats.contradictions;

            println!(
                "{}",
                render::render_action_result(
                    "review_project_complete",
                    &[
                        ("project", project_label.to_string()),
                        ("kept", outcome.stats.kept.to_string()),
                        ("dropped", outcome.stats.dropped.to_string()),
                        ("merged", outcome.stats.merged.to_string()),
                        ("superseded", outcome.stats.superseded.to_string()),
                        ("extracted", outcome.stats.extracted.to_string()),
                        ("contradictions", outcome.stats.contradictions.to_string()),
                        ("failed", outcome.stats.failed.to_string()),
                    ]
                )
            );
            Stage0Outcome {
                survivors: outcome.survivors,
                contradiction_ids: outcome.contradiction_ids,
            }
        }
        Err(e) => {
            // A Stage 0 failure is a single "batch didn't parse" event,
            // not a per-memory failure. We pass the candidates through
            // to Stages A + B, which do their own counting; inflating
            // `failed` here would double-count with Stage B's work.
            tracing::warn!(project = %project_label, error = %e,
                "project review pass failed; falling back to all-keep");
            println!(
                "{}",
                render::render_action_result(
                    "review_failed",
                    &[
                        ("project", project_label.to_string()),
                        ("reason", format!("{e}")),
                    ]
                )
            );
            Stage0Outcome {
                survivors: candidates,
                contradiction_ids: HashSet::new(),
            }
        }
    }
}

/// Stage A — cosine dedup.
///
/// For each candidate we fetch the project's other live rows that share
/// its `memory_type` + `embedding_model` axis (the dedup key), and run
/// [`dedup::find_duplicate`]. Near-matches above `cfg.cosine_threshold`
/// get superseded. Superseded candidates are dropped from the returned
/// survivor list so Stage B doesn't re-condense them.
fn run_stage_a_dedup(
    conn: &Connection,
    cfg: &DreamConfig<'_>,
    candidates: &[Memory],
    contradiction_ids: &HashSet<String>,
    stats: &mut ProjectStats,
) -> Vec<Memory> {
    let mut survivors: Vec<Memory> = Vec::with_capacity(candidates.len());
    for mem in candidates {
        if contradiction_ids.contains(&mem.id) {
            survivors.push(mem.clone());
            continue;
        }
        let peers = match q::list_dedup_candidates(
            conn,
            &mem.id,
            mem.project.as_deref(),
            mem.memory_type.as_deref(),
            mem.embedding_model.as_deref(),
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(id = %mem.id, error = %e,
                    "stage A dedup peer lookup failed; keeping memory");
                survivors.push(mem.clone());
                continue;
            }
        };

        let decision = dedup::find_duplicate(mem, &peers, cfg.cosine_threshold);
        match decision {
            dedup::DedupDecision::Distinct => survivors.push(mem.clone()),
            _ => {
                if cfg.mode == DreamMode::Apply {
                    if let Some(gateway_config) = cfg.gateway_config {
                        if let Some(older) = dedup_superseded_memory(mem, &decision) {
                            if let Err(e) =
                                agent_memory::gateway_sync::tombstone_memory_before_local_removal(
                                    conn,
                                    gateway_config,
                                    &older,
                                    "memory-dream dedup superseded memory",
                                )
                            {
                                stats.failed += 1;
                                tracing::warn!(id = %older.id, error = %e,
                                    "stage A gateway tombstone failed; keeping memory");
                                survivors.push(mem.clone());
                                continue;
                            }
                        }
                    }
                    match dedup::apply_policy(conn, mem, &decision) {
                        Ok(Some((older, newer))) => {
                            if let Err(e) = q::prepare_memory_gateway_sync_for_delete(conn, &older)
                            {
                                stats.failed += 1;
                                tracing::warn!(id = %older, error = %e,
                                    "stage A gateway sync cleanup failed");
                                survivors.push(mem.clone());
                                continue;
                            }
                            stats.superseded += 1;
                            println!(
                                "{}",
                                render::render_action_result(
                                    "dedup_superseded",
                                    &[
                                        ("older", render::short_id(&older).to_string()),
                                        ("newer", render::short_id(&newer).to_string()),
                                    ]
                                )
                            );
                            // If `mem` itself was the loser, drop it from
                            // the survivor list (condensing a row that's
                            // already hidden from default reads is wasted
                            // work). Otherwise keep it — the sibling lost,
                            // `mem` stays live.
                            if older == mem.id {
                                continue;
                            }
                            survivors.push(mem.clone());
                        }
                        Ok(None) => survivors.push(mem.clone()),
                        Err(e) => {
                            stats.failed += 1;
                            tracing::warn!(id = %mem.id, error = %e,
                                "stage A apply_policy failed");
                            survivors.push(mem.clone());
                        }
                    }
                } else {
                    // Dry mode: surface the intent but do not mutate.
                    stats.superseded += 1;
                    println!(
                        "{}",
                        render::render_action_result(
                            "dedup_would_supersede",
                            &[("id", render::short_id(&mem.id).to_string())]
                        )
                    );
                    survivors.push(mem.clone());
                }
            }
        }
    }
    survivors
}

fn dedup_superseded_memory(source: &Memory, decision: &dedup::DedupDecision<'_>) -> Option<Memory> {
    let candidate = match decision {
        dedup::DedupDecision::ExactMatch(candidate) => *candidate,
        dedup::DedupDecision::NearMatch { candidate, .. } => *candidate,
        dedup::DedupDecision::Distinct => return None,
    };
    if candidate.created_at <= source.created_at {
        Some(candidate.clone())
    } else {
        Some(source.clone())
    }
}

/// Stage B — per-memory condense using the three-way response contract.
///
/// Inference and embedding run with NO sqlite lock held; only the final
/// single-statement write (`UPDATE` on rewrite, `DELETE` on forget) touches
/// the DB, and SQLite's implicit per-statement transaction commits it
/// atomically. No `BEGIN IMMEDIATE` is used here — the previous design
/// wrapped the whole per-memory pass (LLM call included) in an immediate
/// transaction, which held a RESERVED write lock for the full LLM round
/// trip and stalled concurrent `memory store`/`update`/`forget` for the
/// entire pass. See Issue: DB-lock-during-dream.
fn run_stage_b_condense(
    conn: &Connection,
    inference: &dyn Inference,
    cfg: &DreamConfig<'_>,
    source: &Memory,
    stats: &mut ProjectStats,
) {
    // Belt-and-braces — candidate list already filters superseded rows.
    if source.superseded_by.is_some() {
        stats.kept += 1;
        return;
    }

    // Inference — slow, lock-free.
    let okf_context = project_review::okf_review_context(conn, std::slice::from_ref(source)).ok();
    let outcome = condense::run_per_memory_with_context(inference, source, okf_context.as_deref());

    match outcome {
        Ok(condense::Decision::Skip) => {
            stats.kept += 1;
            if cfg.mode == DreamMode::Apply {
                // A skip is the condenser reading the body and finding nothing
                // to improve — the same confirmation the review pass records.
                if let Err(e) =
                    project_review::record_curation_review(conn, &source.id, cfg.model_name)
                {
                    tracing::warn!(id = %source.id, error = %e,
                        "condense skip confirmation failed");
                }
            }
            println!(
                "{}",
                render::render_action_result(
                    "kept",
                    &[("id", render::short_id(&source.id).to_string())]
                )
            );
        }
        Ok(condense::Decision::Forget) => {
            if cfg.mode == DreamMode::Apply {
                if let Some(gateway_config) = cfg.gateway_config {
                    if let Err(e) =
                        agent_memory::gateway_sync::tombstone_memory_before_local_removal(
                            conn,
                            gateway_config,
                            source,
                            "memory-dream condense forgot memory",
                        )
                    {
                        stats.failed += 1;
                        println!(
                            "{}",
                            render::render_action_result(
                                "condense_failed",
                                &[
                                    ("id", render::short_id(&source.id).to_string()),
                                    ("reason", format!("gateway tombstone: {e}")),
                                ]
                            )
                        );
                        return;
                    }
                }
                match agent_memory::db::queries::delete_memory_with_actor(
                    conn,
                    &source.id,
                    Some("memory-dream"),
                    Some("condense forget"),
                ) {
                    Ok(_) => {
                        stats.forgot += 1;
                        println!(
                            "{}",
                            render::render_action_result(
                                "forgot",
                                &[("id", render::short_id(&source.id).to_string())]
                            )
                        );
                    }
                    Err(e) => {
                        stats.failed += 1;
                        println!(
                            "{}",
                            render::render_action_result(
                                "condense_failed",
                                &[
                                    ("id", render::short_id(&source.id).to_string()),
                                    ("reason", format!("delete: {e}")),
                                ]
                            )
                        );
                    }
                }
            } else {
                stats.forgot += 1;
                println!(
                    "{}",
                    render::render_action_result(
                        "would_forget",
                        &[("id", render::short_id(&source.id).to_string())]
                    )
                );
            }
        }
        Ok(condense::Decision::Rewrite { text }) => {
            let bytes_before = source.content.len();
            let bytes_after = text.len();

            // Re-embed the condensed content so vector search doesn't
            // drift from the visible text. Embedding is CPU-bound and
            // runs lock-free — fastembed never touches SQLite.
            let new_emb = match embed_text(&text, cfg.embedding_cache_dir) {
                Ok(v) => v,
                Err(e) => {
                    stats.failed += 1;
                    println!(
                        "{}",
                        render::render_action_result(
                            "condense_failed",
                            &[
                                ("id", render::short_id(&source.id).to_string()),
                                ("reason", format!("embed: {e}")),
                            ]
                        )
                    );
                    return;
                }
            };

            if cfg.mode == DreamMode::Apply {
                // Preserve the ORIGINAL raw body: update_content uses
                // COALESCE(content_raw, content) so a re-condensation
                // never chains through an intermediate condensed form.
                match q::update_condensation(
                    conn,
                    &source.id,
                    &text,
                    source.content_raw.as_deref().unwrap_or(&source.content),
                    &prompt::condenser_version_stamp(cfg.model_name),
                    &generated_by(cfg.model_name),
                    &new_emb,
                    EMBEDDING_MODEL_NAME,
                ) {
                    Ok(()) => {
                        if let Some(gateway_config) = cfg.gateway_config {
                            match q::get_memory_by_id(conn, &source.id).and_then(|memory| {
                                agent_memory::gateway_sync::push_memory_update_if_configured(
                                    conn,
                                    gateway_config,
                                    &memory,
                                )
                            }) {
                                Ok(_) => {}
                                Err(e) => {
                                    stats.failed += 1;
                                    println!(
                                        "{}",
                                        render::render_action_result(
                                            "condense_gateway_sync_failed",
                                            &[
                                                ("id", render::short_id(&source.id).to_string()),
                                                ("reason", format!("{e}")),
                                            ]
                                        )
                                    );
                                }
                            }
                        }
                        stats.rewritten += 1;
                        println!(
                            "{}",
                            render::render_action_result(
                                "rewritten",
                                &[
                                    ("id", render::short_id(&source.id).to_string()),
                                    ("bytes_before", bytes_before.to_string()),
                                    ("bytes_after", bytes_after.to_string()),
                                ]
                            )
                        );
                    }
                    Err(e) => {
                        stats.failed += 1;
                        println!(
                            "{}",
                            render::render_action_result(
                                "condense_failed",
                                &[
                                    ("id", render::short_id(&source.id).to_string()),
                                    ("reason", format!("update: {e}")),
                                ]
                            )
                        );
                    }
                }
            } else {
                stats.rewritten += 1;
                println!(
                    "{}",
                    render::render_action_result(
                        "would_rewrite",
                        &[
                            ("id", render::short_id(&source.id).to_string()),
                            ("bytes_before", bytes_before.to_string()),
                            ("bytes_after", bytes_after.to_string()),
                        ]
                    )
                );
            }
        }
        Err(e) => {
            stats.failed += 1;
            tracing::info!(id = %source.id, error = %e, "stage B condense failed");
            println!(
                "{}",
                render::render_action_result(
                    "condense_failed",
                    &[
                        ("id", render::short_id(&source.id).to_string()),
                        ("reason", format!("{e}")),
                    ]
                )
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::FixedInference;
    use std::path::PathBuf;

    fn open_mem_db() -> Connection {
        agent_memory::db::open_database(&PathBuf::from(":memory:")).expect("open in-memory db")
    }

    fn insert(conn: &Connection, id: &str, content: &str, project: Option<&str>) {
        let mut m = Memory::new(
            content.to_string(),
            None,
            project.map(String::from),
            None,
            None,
            Some("user".to_string()),
        );
        m.id = id.to_string();
        m.embedding_model = Some(EMBEDDING_MODEL_NAME.to_string());
        q::insert_memory(conn, &m).expect("insert");
    }

    /// Zero-memory DB: the orchestrator exits cleanly with zero counts.
    #[test]
    fn empty_db_exits_cleanly() {
        let mut conn = open_mem_db();
        let inf = FixedInference::new("skip");
        let tmp = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        let summary = run(&mut conn, &inf, &cfg).expect("dream ok");
        assert_eq!(summary.total_walked, 0);
        assert_eq!(summary.kept, 0);
        assert_eq!(summary.rewritten, 0);
        assert_eq!(summary.forgot, 0);
    }

    #[test]
    fn working_context_rows_do_not_create_dream_candidates() {
        let mut conn = open_mem_db();
        q::set_working_context(&conn, "p1", "transient handoff").unwrap();

        let inf = FixedInference::new("skip");
        let tmp = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        let summary = run(&mut conn, &inf, &cfg).expect("dream ok");

        assert_eq!(summary.total_walked, 0);
        assert!(q::get_working_context(&conn, "p1").unwrap().is_some());
    }

    /// Incremental filter: after a successful pass, re-running with no new
    /// writes should yield zero candidates.
    #[test]
    fn incremental_filter_skips_processed_projects() {
        let mut conn = open_mem_db();
        insert_already_processed(&conn, "aaaaaaaa-0000-1111-2222-000000000001", "first", "p1");

        let inf = FixedInference::new("skip");
        let tmp = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        let summary = run(&mut conn, &inf, &cfg).expect("dream ok");
        assert_eq!(summary.total_walked, 0, "incremental filter must skip rows");
    }

    /// --full flag overrides the incremental cutoff — every row is walked.
    #[test]
    fn full_flag_re_walks_everything() {
        let mut conn = open_mem_db();
        insert(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000001",
            "first",
            Some("p1"),
        );
        q::set_last_dream_at(&conn, Some("p1"), "2099-01-01T00:00:00Z").unwrap();

        let inf = FixedInference::new("skip");
        let tmp = std::env::temp_dir();
        let mut cfg = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        cfg.full = true;
        let summary = run(&mut conn, &inf, &cfg).expect("dream ok");
        assert_eq!(summary.total_walked, 1, "--full must ignore cutoff");
    }

    #[test]
    fn limit_is_global_across_projects() {
        let mut conn = open_mem_db();
        for (id, project) in [
            ("aaaaaaaa-0000-1111-2222-000000000101", "p1"),
            ("aaaaaaaa-0000-1111-2222-000000000102", "p1"),
            ("aaaaaaaa-0000-1111-2222-000000000103", "p2"),
            ("aaaaaaaa-0000-1111-2222-000000000104", "p2"),
        ] {
            insert(&conn, id, id, Some(project));
        }
        let inf = FixedInference::new("skip");
        let tmp = std::env::temp_dir();
        let mut cfg = DreamConfig::new(DreamMode::Apply, "test/model", &tmp);
        cfg.limit = 3;
        let summary = run(&mut conn, &inf, &cfg).unwrap();
        assert_eq!(summary.total_walked, 3);
    }

    /// Helper for the --refresh stage tests: insert a memory that would
    /// normally be skipped by the incremental gate (stamped with the
    /// current condenser_version AND updated_at older than
    /// project_state.last_dream_at). Returns the memory id.
    ///
    /// The three tests below drive this shape through the pipeline with
    /// and without `cfg.full = true` to confirm every stage honors the
    /// --refresh override (which folds into `cfg.full` in `main.rs`).
    fn insert_already_processed(conn: &Connection, id: &str, content: &str, project: &str) {
        let stamp = prompt::condenser_version_stamp("sonnet");
        let mut memory = Memory::new(
            content.to_string(),
            None,
            Some(project.to_string()),
            None,
            None,
            Some("user".to_string()),
        );
        memory.id = id.to_string();
        memory.created_at = "2026-01-01T00:00:00Z".to_string();
        memory.updated_at = memory.created_at.clone();
        memory.condenser_version = Some(stamp);
        memory.embedding_model = Some(EMBEDDING_MODEL_NAME.to_string());
        q::insert_memory(conn, &memory).unwrap();
        q::set_last_dream_at(conn, Some(project), "2099-01-01T00:00:00Z").unwrap();
    }

    /// --refresh (via `cfg.full`) re-feeds an already-processed memory
    /// into Stage 0 (project review). Without it, the memory is skipped
    /// by the incremental gate in `list_dream_candidates` and Stage 0
    /// never sees it. The test asserts Stage 0 actually ran by checking
    /// `review_kept > 0` in the summary — that counter is only bumped
    /// inside project_review::run_project.
    #[test]
    fn refresh_flag_reruns_stage_0_project_review() {
        let mut conn = open_mem_db();
        let id = "aaaaaaaa-0000-1111-2222-000000000010";
        insert_already_processed(&conn, id, "already reviewed", "p1");

        // Canned response that keeps the memory so it survives into
        // Stage A and B (we only care that Stage 0 fired).
        let canned = format!(
            r#"{{"decisions": {{"{id}": {{"action": "keep"}}}}}}"#,
            id = id
        );
        let inf = FixedInference::new(canned);
        let tmp = std::env::temp_dir();

        // Sanity: without --refresh the memory is skipped entirely.
        let cfg_skip = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        let summary_skip = run(&mut conn, &inf, &cfg_skip).expect("dream ok");
        assert_eq!(
            summary_skip.total_walked, 0,
            "precondition: incremental gate must hide the memory by default"
        );
        assert_eq!(
            summary_skip.review_kept, 0,
            "Stage 0 must not fire by default"
        );

        // With --refresh the memory re-enters the pipeline and Stage 0
        // sees it.
        let mut cfg_refresh = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        cfg_refresh.full = true;
        let summary_refresh = run(&mut conn, &inf, &cfg_refresh).expect("dream ok");
        assert!(
            summary_refresh.review_kept >= 1,
            "--refresh must let Stage 0 (project review) reprocess the memory; got review_kept={}",
            summary_refresh.review_kept
        );
    }

    /// --refresh re-feeds already-processed memories into Stage A
    /// (cosine dedup). Two byte-identical rows are pre-stamped so the
    /// default gate skips them; with refresh on, Stage A fires and
    /// supersedes the older row.
    #[test]
    fn refresh_flag_reruns_stage_a_dedup() {
        let mut conn = open_mem_db();
        let older = "aaaaaaaa-0000-1111-2222-000000000020";
        let newer = "aaaaaaaa-0000-1111-2222-000000000021";

        // Two byte-identical rows in the same project / memory_type /
        // embedding_model so Stage A's exact-match short-circuit fires.
        insert_already_processed(&conn, older, "identical content", "p2");
        insert_already_processed(&conn, newer, "identical content", "p2");
        conn.execute(
            "UPDATE memories SET created_at = ?1, updated_at = ?1 WHERE id = ?2",
            rusqlite::params!["2026-01-02T00:00:00Z", newer],
        )
        .unwrap();
        q::set_last_dream_at(&conn, Some("p2"), "2099-01-01T00:00:00Z").unwrap();

        // Stage 0 response must keep both memories so they survive into
        // Stage A where dedup actually runs.
        let canned = format!(
            r#"{{"decisions": {{
                "{older}": {{"action": "keep"}},
                "{newer}": {{"action": "keep"}}
            }}}}"#
        );
        let inf = FixedInference::new(canned);
        let tmp = std::env::temp_dir();

        // Without --refresh: incremental gate hides both → no dedup work.
        let cfg_skip = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        let summary_skip = run(&mut conn, &inf, &cfg_skip).expect("dream ok");
        assert_eq!(
            summary_skip.superseded, 0,
            "precondition: Stage A must not fire by default on pre-stamped rows"
        );

        // With --refresh: Stage A fires and supersedes the older row.
        let mut cfg_refresh = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        cfg_refresh.full = true;
        let summary_refresh = run(&mut conn, &inf, &cfg_refresh).expect("dream ok");
        assert!(
            summary_refresh.superseded >= 1,
            "--refresh must let Stage A (cosine dedup) reprocess; got superseded={}",
            summary_refresh.superseded
        );
    }

    /// --refresh re-feeds an already-processed memory into Stage B
    /// (per-memory condense).
    ///
    /// Wiring note: the orchestrator uses one [`FixedInference`] stub for
    /// both Stage 0 (JSON project-review response) and Stage B (free-text
    /// three-way contract). We pad the memory body with enough filler so
    /// the JSON payload is comfortably below the Stage B length ceiling
    /// (baseline + REWRITE_CHAR_SLACK) — the parser then accepts it as a
    /// rewrite, incrementing `rewritten`. That bump is the signal Stage B
    /// fired; without --refresh it stays at zero because the incremental
    /// gate hides the memory.
    #[test]
    fn refresh_flag_reruns_stage_b_condense() {
        let mut conn = open_mem_db();
        let id = "aaaaaaaa-0000-1111-2222-000000000030";

        // Pad the memory body so any plausible JSON payload sits below
        // the Stage B length ceiling (baseline + REWRITE_CHAR_SLACK).
        // 300 chars is comfortably above the ~70-char keep-decision JSON
        // below even after adding the slack.
        let long_content = "x".repeat(300);
        insert_already_processed(&conn, id, &long_content, "p3");

        // Stage 0 keeps the memory; Stage B gets the same string and
        // treats it as a (shorter than input) rewrite body.
        let canned = format!(
            r#"{{"decisions": {{"{id}": {{"action": "keep"}}}}}}"#,
            id = id
        );
        let inf = FixedInference::new(canned);
        let tmp = std::env::temp_dir();

        // Without --refresh: Stage B never fires.
        let cfg_skip = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        let summary_skip = run(&mut conn, &inf, &cfg_skip).expect("dream ok");
        let stage_b_touched_default =
            summary_skip.kept + summary_skip.rewritten + summary_skip.forgot;
        assert_eq!(
            stage_b_touched_default, 0,
            "precondition: Stage B must not fire by default"
        );

        // With --refresh: Stage B fires. Some non-failure outcome lands
        // in kept/rewritten/forgot; we don't care which — only that
        // Stage B ran at all.
        let mut cfg_refresh = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        cfg_refresh.full = true;
        let summary_refresh = run(&mut conn, &inf, &cfg_refresh).expect("dream ok");
        let stage_b_touched =
            summary_refresh.kept + summary_refresh.rewritten + summary_refresh.forgot;
        assert!(
            stage_b_touched >= 1,
            "--refresh must let Stage B (per-memory condense) reprocess; \
             kept={}, rewritten={}, forgot={}, failed={}",
            summary_refresh.kept,
            summary_refresh.rewritten,
            summary_refresh.forgot,
            summary_refresh.failed,
        );
    }

    /// `skip` response keeps the memory untouched.
    #[test]
    fn skip_response_keeps_memory() {
        let mut conn = open_mem_db();
        let id = "aaaaaaaa-0000-1111-2222-000000000001";
        insert(&conn, id, "already concise", Some("p1"));

        let inf = FixedInference::new("skip");
        let tmp = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        let summary = run(&mut conn, &inf, &cfg).expect("dream ok");
        assert_eq!(summary.kept, 1);
        assert_eq!(summary.rewritten, 0);
        assert_eq!(summary.forgot, 0);

        // Memory still present.
        let got = q::get_memory_by_id(&conn, id).unwrap();
        assert_eq!(got.content, "already concise");
    }

    /// `forget` response deletes the memory via the DB layer.
    #[test]
    fn forget_response_deletes_memory() {
        let mut conn = open_mem_db();
        let id = "aaaaaaaa-0000-1111-2222-000000000001";
        insert(&conn, id, "CI notification noise", Some("p1"));

        let inf = FixedInference::new("forget");
        let tmp = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        let summary = run(&mut conn, &inf, &cfg).expect("dream ok");
        assert_eq!(summary.forgot, 1);
        assert_eq!(summary.kept, 0);

        // Memory is gone.
        let err = q::get_memory_by_id(&conn, id).unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
        let actor: String = conn
            .query_row(
                "SELECT actor FROM memory_concept_tombstones WHERE memory_id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(actor, "memory-dream");
    }

    /// `forget` dry-run surfaces intent without deleting.
    #[test]
    fn forget_response_dry_run_does_not_delete() {
        let mut conn = open_mem_db();
        let id = "aaaaaaaa-0000-1111-2222-000000000001";
        insert(&conn, id, "CI notification noise", Some("p1"));
        let before: (i64, i64, i64, String) = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM memory_revisions),
                    (SELECT COUNT(*) FROM memory_relationships),
                    (SELECT COUNT(*) FROM memory_concept_tombstones),
                    (SELECT updated_at FROM memories WHERE id = ?1)",
                rusqlite::params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        let inf = FixedInference::new("forget");
        let tmp = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Dry, "sonnet", &tmp);
        let summary = run(&mut conn, &inf, &cfg).expect("dream ok");
        assert_eq!(summary.forgot, 1, "counts still reflect intent");

        // Row must still exist — dry run doesn't delete.
        assert!(q::get_memory_by_id(&conn, id).is_ok());
        let after: (i64, i64, i64, String) = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM memory_revisions),
                    (SELECT COUNT(*) FROM memory_relationships),
                    (SELECT COUNT(*) FROM memory_concept_tombstones),
                    (SELECT updated_at FROM memories WHERE id = ?1)",
                rusqlite::params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn condensation_revision_preserves_okf_provenance_and_clears_verification() {
        let conn = open_mem_db();
        let id = "aaaaaaaa-0000-1111-2222-000000000041";
        let peer = "aaaaaaaa-0000-1111-2222-000000000042";
        insert(&conn, id, "verbose original body", Some("p1"));
        insert(&conn, peer, "related", Some("p1"));
        agent_memory::concepts::mutate(
            &conn,
            id,
            "enrich",
            Some("test"),
            None,
            false,
            |revision| {
                conn.execute(
                    "UPDATE memory_concepts SET extensions_json = '{\"x-producer\":{\"keep\":true}}'
                     WHERE memory_id = ?1",
                    rusqlite::params![id],
                )?;
                conn.execute(
                    "INSERT INTO memory_sources (
                         memory_id, source_key, ordinal, resource, metadata_json
                     ) VALUES (?1, 'source-1', 0, 'https://example.invalid/source', '{}')",
                    rusqlite::params![id],
                )?;
                conn.execute(
                    "INSERT INTO memory_verifications (
                         id, memory_id, actor, verified_at, metadata_json
                     ) VALUES ('verification-1', ?1, 'reviewer', '2026-08-15T00:00:00Z', '{}')",
                    rusqlite::params![id],
                )?;
                agent_memory::concepts::insert_relationship(
                    &conn,
                    id,
                    Some(peer),
                    peer,
                    "supports",
                    revision,
                    None,
                )
            },
        )
        .unwrap();

        q::update_condensation(
            &conn,
            id,
            "concise body",
            "verbose original body",
            "test:stamp",
            &generated_by("test-model"),
            &[1.0, 0.0],
            EMBEDDING_MODEL_NAME,
        )
        .unwrap();

        let state: (String, i64, i64, i64, i64, String, String) = conn
            .query_row(
                "SELECT c.extensions_json, c.current_revision,
                        (SELECT COUNT(*) FROM memory_sources WHERE memory_id = ?1),
                        (SELECT COUNT(*) FROM memory_relationships WHERE src_memory_id = ?1),
                        (SELECT COUNT(*) FROM memory_verifications WHERE memory_id = ?1),
                        r.operation, r.actor
                 FROM memory_concepts c
                 JOIN memory_revisions r ON r.memory_id = c.memory_id
                      AND r.revision = c.current_revision
                 WHERE c.memory_id = ?1",
                rusqlite::params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert!(state.0.contains("x-producer"));
        assert_eq!(state.1, 3);
        assert_eq!(state.2, 1);
        assert_eq!(state.3, 1);
        assert_eq!(state.4, 0);
        assert_eq!(state.5, "dream_condense");
        assert_eq!(state.6, "memory-dream");
    }

    /// Apply-mode pass stamps `project_state.last_dream_at`.
    #[test]
    fn apply_mode_stamps_project_state() {
        let mut conn = open_mem_db();
        insert(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000001",
            "first",
            Some("p1"),
        );

        let inf = FixedInference::new("skip");
        let tmp = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        run(&mut conn, &inf, &cfg).expect("dream ok");

        let ts = q::get_last_dream_at(&conn, Some("p1"))
            .unwrap()
            .expect("project_state row must exist after apply");
        assert!(!ts.is_empty());
    }

    /// Dry-mode pass does NOT stamp `project_state`.
    #[test]
    fn dry_mode_does_not_stamp_project_state() {
        let mut conn = open_mem_db();
        insert(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000001",
            "first",
            Some("p1"),
        );

        let inf = FixedInference::new("skip");
        let tmp = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Dry, "sonnet", &tmp);
        run(&mut conn, &inf, &cfg).expect("dream ok");

        let ts = q::get_last_dream_at(&conn, Some("p1")).unwrap();
        assert!(ts.is_none(), "dry-run must not stamp project_state");
    }

    /// A malformed response (neither `skip`/`forget` nor shorter than
    /// input) lands in the failed bucket and the row survives unchanged.
    #[test]
    fn malformed_response_is_counted_as_failed() {
        let mut conn = open_mem_db();
        let id = "aaaaaaaa-0000-1111-2222-000000000001";
        insert(&conn, id, "short", Some("p1"));

        // Inference stub returns a string LONGER than the input — the
        // length-guard in condense should reject it.
        let inf = FixedInference::new("this is a much longer response than the raw input");
        let tmp = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Apply, "sonnet", &tmp);
        let summary = run(&mut conn, &inf, &cfg).expect("dream ok");
        assert_eq!(summary.failed, 1);

        // Row still there with original content.
        let got = q::get_memory_by_id(&conn, id).unwrap();
        assert_eq!(got.content, "short");
    }
}
