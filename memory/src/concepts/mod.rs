//! Transactional persistence for OKF-native durable memories.
//!
//! This module is the write boundary between the legacy `memories` columns
//! and their canonical OKF metadata/history.  It deliberately performs no
//! embedding, inference, filesystem, or network work.

#[allow(dead_code)]
pub mod graph;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::db::models::Memory;
use crate::error::MemoryError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionOutcome {
    pub changed: bool,
    pub revision: i64,
    pub content_hash: String,
}

#[derive(Serialize)]
struct SemanticSnapshot {
    body: String,
    content_raw: Option<String>,
    tags: serde_json::Value,
    project: Option<String>,
    agent: Option<String>,
    source_file: Option<String>,
    memory_type: String,
    superseded_by: Option<String>,
    condenser_version: Option<String>,
    okf: ConceptSnapshot,
    sources: Vec<SourceSnapshot>,
    verifications: Vec<VerificationSnapshot>,
    relationships: Vec<RelationshipSnapshot>,
}

#[derive(Serialize)]
struct ConceptSnapshot {
    #[serde(rename = "type")]
    concept_type: String,
    title: Option<String>,
    description: Option<String>,
    resource: Option<String>,
    status: String,
    stale_after: Option<String>,
    generated_by: Option<String>,
    generated_at: Option<String>,
    extensions: serde_json::Value,
    #[serde(rename = "x-agent-memory")]
    agent_memory: AgentMemorySnapshot,
}

#[derive(Serialize)]
struct AgentMemorySnapshot {
    id: String,
    memory_type: String,
    project: Option<String>,
    scope: String,
}

#[derive(Serialize)]
struct SourceSnapshot {
    source_key: String,
    ordinal: i64,
    resource: String,
    title: Option<String>,
    author: Option<String>,
    usage_count: Option<i64>,
    usage_window_from: Option<String>,
    usage_window_to: Option<String>,
    last_modified: Option<String>,
    metadata: serde_json::Value,
}

#[derive(Serialize)]
struct VerificationSnapshot {
    actor: String,
    verified_at: String,
    verification_kind: Option<String>,
    metadata: serde_json::Value,
}

#[derive(Serialize)]
struct RelationshipSnapshot {
    dst_memory_id: Option<String>,
    dst_ref: String,
    relation: String,
    confidence: String,
    producer: String,
    ordinal: Option<i64>,
    metadata: serde_json::Value,
}

fn json_or_string(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

fn normalized_tags(raw: Option<&str>) -> serde_json::Value {
    let Some(raw) = raw else {
        return serde_json::Value::Null;
    };
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(mut tags) => {
            tags.sort();
            tags.dedup();
            serde_json::to_value(tags).expect("serializing strings cannot fail")
        }
        Err(_) => json_or_string(raw),
    }
}

fn memory_scope(project: Option<&str>) -> &'static str {
    match project {
        Some("__global__") => "global",
        Some(_) => "project",
        None => "unscoped",
    }
}

fn default_memory_type(memory_type: Option<&str>) -> &str {
    memory_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("user")
}

fn savepoint_name() -> String {
    format!("okf_{}", uuid::Uuid::new_v4().simple())
}

fn in_savepoint<T>(
    conn: &Connection,
    operation: impl FnOnce() -> Result<T, MemoryError>,
) -> Result<T, MemoryError> {
    let name = savepoint_name();
    conn.execute_batch(&format!("SAVEPOINT {name}"))?;
    match operation() {
        Ok(value) => {
            conn.execute_batch(&format!("RELEASE SAVEPOINT {name}"))?;
            Ok(value)
        }
        Err(error) => {
            let _ = conn.execute_batch(&format!(
                "ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"
            ));
            Err(error)
        }
    }
}

/// Group several concept operations into one nested-safe atomic unit.
pub fn atomic<T>(
    conn: &Connection,
    operation: impl FnOnce() -> Result<T, MemoryError>,
) -> Result<T, MemoryError> {
    in_savepoint(conn, operation)
}

fn snapshot(conn: &Connection, memory_id: &str) -> Result<SemanticSnapshot, MemoryError> {
    let mut base = conn.query_row(
        "SELECT m.content, m.content_raw, m.tags, m.project, m.agent, m.source_file,
                m.memory_type, m.superseded_by, m.condenser_version,
                c.concept_type, c.title, c.description, c.resource, c.status,
                c.stale_after, c.generated_by, c.generated_at, c.extensions_json
         FROM memories m JOIN memory_concepts c ON c.memory_id = m.id
         WHERE m.id = ?1",
        params![memory_id],
        |row| {
            let tags: Option<String> = row.get(2)?;
            let project: Option<String> = row.get(3)?;
            let memory_type: Option<String> = row.get(6)?;
            let extensions: String = row.get(17)?;
            let normalized_type = default_memory_type(memory_type.as_deref()).to_string();
            Ok(SemanticSnapshot {
                body: row.get(0)?,
                content_raw: row.get(1)?,
                tags: normalized_tags(tags.as_deref()),
                project: project.clone(),
                agent: row.get(4)?,
                source_file: row.get(5)?,
                memory_type: normalized_type.clone(),
                superseded_by: row.get(7)?,
                condenser_version: row.get(8)?,
                okf: ConceptSnapshot {
                    concept_type: row.get(9)?,
                    title: row.get(10)?,
                    description: row.get(11)?,
                    resource: row.get(12)?,
                    status: row.get(13)?,
                    stale_after: row.get(14)?,
                    generated_by: row.get(15)?,
                    generated_at: row.get(16)?,
                    extensions: json_or_string(&extensions),
                    agent_memory: AgentMemorySnapshot {
                        id: memory_id.to_string(),
                        memory_type: normalized_type,
                        project: project.clone(),
                        scope: memory_scope(project.as_deref()).to_string(),
                    },
                },
                sources: Vec::new(),
                verifications: Vec::new(),
                relationships: Vec::new(),
            })
        },
    )?;

    let mut stmt = conn.prepare(
        "SELECT source_key, ordinal, resource, title, author, usage_count,
                usage_window_from, usage_window_to, last_modified, metadata_json
         FROM memory_sources WHERE memory_id = ?1 ORDER BY ordinal, source_key",
    )?;
    base.sources = stmt
        .query_map(params![memory_id], |row| {
            let metadata: String = row.get(9)?;
            Ok(SourceSnapshot {
                source_key: row.get(0)?,
                ordinal: row.get(1)?,
                resource: row.get(2)?,
                title: row.get(3)?,
                author: row.get(4)?,
                usage_count: row.get(5)?,
                usage_window_from: row.get(6)?,
                usage_window_to: row.get(7)?,
                last_modified: row.get(8)?,
                metadata: json_or_string(&metadata),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut stmt = conn.prepare(
        "SELECT id, actor, verified_at, verification_kind, metadata_json
         FROM memory_verifications WHERE memory_id = ?1 ORDER BY verified_at, id",
    )?;
    base.verifications = stmt
        .query_map(params![memory_id], |row| {
            let metadata: String = row.get(4)?;
            Ok(VerificationSnapshot {
                actor: row.get(1)?,
                verified_at: row.get(2)?,
                verification_kind: row.get(3)?,
                metadata: json_or_string(&metadata),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut stmt = conn.prepare(
        "SELECT id, dst_memory_id, dst_ref, relation, confidence, producer,
                ordinal, metadata_json
         FROM memory_relationships WHERE src_memory_id = ?1
         ORDER BY relation, dst_ref, producer, COALESCE(ordinal, -1), id",
    )?;
    base.relationships = stmt
        .query_map(params![memory_id], |row| {
            let metadata: String = row.get(7)?;
            Ok(RelationshipSnapshot {
                dst_memory_id: row.get(1)?,
                dst_ref: row.get(2)?,
                relation: row.get(3)?,
                confidence: row.get(4)?,
                producer: row.get(5)?,
                ordinal: row.get(6)?,
                metadata: json_or_string(&metadata),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(base)
}

fn encode_snapshot(value: &SemanticSnapshot) -> Result<(String, String), MemoryError> {
    let json = serde_json::to_string(value)?;
    let hash = format!("{:x}", Sha256::digest(json.as_bytes()));
    Ok((json, hash))
}

pub fn current_revision(conn: &Connection, memory_id: &str) -> Result<i64, MemoryError> {
    conn.query_row(
        "SELECT current_revision FROM memory_concepts WHERE memory_id = ?1",
        params![memory_id],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| MemoryError::NotFound(memory_id.to_string()))
}

/// One concept's descriptive OKF projection, captured so a replacement
/// concept can inherit it.
///
/// Curation that replaces a memory with a rewritten one (Dream's supersede
/// and extract decisions) destroys the original row. Everything the concept
/// had accumulated — domain type, title, description, lifecycle, sources,
/// unknown round-tripped frontmatter — is not re-derivable from the new body,
/// so it has to be carried across explicitly or it is silently lost.
///
/// Deliberately excluded:
/// - `generated_by`/`generated_at`, which describe who produced *this* body;
/// - verifications, because a meaningful body change invalidates them;
/// - `virtual_path`, `current_revision`, timestamps, all identity-bound.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct CarriedConcept {
    pub concept_type: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub resource: Option<String>,
    pub status: String,
    pub stale_after: Option<String>,
    pub extensions_json: String,
    pub raw_frontmatter: Option<String>,
    pub sources: Vec<CarriedSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct CarriedSource {
    pub source_key: String,
    pub ordinal: i64,
    pub resource: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub usage_count: Option<i64>,
    pub usage_window_from: Option<String>,
    pub usage_window_to: Option<String>,
    pub last_modified: Option<String>,
    pub metadata_json: String,
}

/// Capture a concept's carryable OKF projection. Call before the delete that
/// cascades it away. Returns `None` when the memory has no concept row.
#[allow(dead_code)]
pub fn capture_concept(
    conn: &Connection,
    memory_id: &str,
) -> Result<Option<CarriedConcept>, MemoryError> {
    let captured = conn
        .query_row(
            "SELECT concept_type, title, description, resource, status, stale_after,
                    extensions_json, raw_frontmatter
             FROM memory_concepts WHERE memory_id = ?1",
            params![memory_id],
            |row| {
                Ok(CarriedConcept {
                    concept_type: row.get(0)?,
                    title: row.get(1)?,
                    description: row.get(2)?,
                    resource: row.get(3)?,
                    status: row.get(4)?,
                    stale_after: row.get(5)?,
                    extensions_json: row.get(6)?,
                    raw_frontmatter: row.get(7)?,
                    sources: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(mut captured) = captured else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT source_key, ordinal, resource, title, author, usage_count,
                usage_window_from, usage_window_to, last_modified, metadata_json
         FROM memory_sources WHERE memory_id = ?1 ORDER BY ordinal, source_key",
    )?;
    let rows = stmt.query_map(params![memory_id], |row| {
        Ok(CarriedSource {
            source_key: row.get(0)?,
            ordinal: row.get(1)?,
            resource: row.get(2)?,
            title: row.get(3)?,
            author: row.get(4)?,
            usage_count: row.get(5)?,
            usage_window_from: row.get(6)?,
            usage_window_to: row.get(7)?,
            last_modified: row.get(8)?,
            metadata_json: row.get(9)?,
        })
    })?;
    for row in rows {
        captured.sources.push(row?);
    }
    Ok(Some(captured))
}

/// Apply a captured projection onto a concept. Intended to run inside the
/// `configure` callback of [`insert_memory_with`] so the carried metadata is
/// part of the replacement's revision 1 snapshot rather than a later edit.
#[allow(dead_code)]
pub fn apply_carried_concept(
    conn: &Connection,
    memory_id: &str,
    carried: &CarriedConcept,
) -> Result<(), MemoryError> {
    conn.execute(
        "UPDATE memory_concepts SET
             concept_type = ?1, title = ?2, description = ?3, resource = ?4,
             status = ?5, stale_after = ?6, extensions_json = ?7, raw_frontmatter = ?8
         WHERE memory_id = ?9",
        params![
            carried.concept_type,
            carried.title,
            carried.description,
            carried.resource,
            carried.status,
            carried.stale_after,
            carried.extensions_json,
            carried.raw_frontmatter,
            memory_id,
        ],
    )?;
    for source in &carried.sources {
        conn.execute(
            "INSERT OR REPLACE INTO memory_sources (
                 memory_id, source_key, ordinal, resource, title, author, usage_count,
                 usage_window_from, usage_window_to, last_modified, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                memory_id,
                source.source_key,
                source.ordinal,
                source.resource,
                source.title,
                source.author,
                source.usage_count,
                source.usage_window_from,
                source.usage_window_to,
                source.last_modified,
                source.metadata_json,
            ],
        )?;
    }
    Ok(())
}

/// Insert a durable memory and its canonical concept/revision atomically.
pub fn insert_memory(
    conn: &Connection,
    memory: &Memory,
    operation: &str,
    actor: Option<&str>,
    derived_from: Option<&str>,
) -> Result<(), MemoryError> {
    insert_memory_with(conn, memory, operation, actor, derived_from, |_| Ok(()))
}

/// Insert a memory while atomically configuring its complete OKF projection.
pub fn insert_memory_with<F>(
    conn: &Connection,
    memory: &Memory,
    operation: &str,
    actor: Option<&str>,
    derived_from: Option<&str>,
    configure: F,
) -> Result<(), MemoryError>
where
    F: FnOnce(i64) -> Result<(), MemoryError>,
{
    in_savepoint(conn, || {
        let tags_json = memory
            .tags
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let embedding_blob = memory
            .embedding
            .as_ref()
            .map(|embedding| crate::db::models::embedding_to_blob(embedding));
        conn.execute(
            "INSERT INTO memories (id, content, tags, project, agent, source_file,
             created_at, updated_at, access_count, embedding, memory_type,
             content_raw, superseded_by, condenser_version, embedding_model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                memory.id,
                memory.content,
                tags_json,
                memory.project,
                memory.agent,
                memory.source_file,
                memory.created_at,
                memory.updated_at,
                memory.access_count,
                embedding_blob,
                memory.memory_type,
                memory.content_raw,
                memory.superseded_by,
                memory.condenser_version,
                memory.embedding_model,
            ],
        )?;
        let memory_type = default_memory_type(memory.memory_type.as_deref());
        conn.execute(
            "INSERT INTO memory_concepts (
                 memory_id, concept_type, status, generated_by, generated_at,
                 extensions_json, current_revision, virtual_path, created_at, updated_at
             ) VALUES (?1, ?2, 'stable', ?3, ?4, '{}', 1, ?5, ?6, ?7)",
            params![
                memory.id,
                format!("Agent Memory/{memory_type}"),
                actor.or(memory.agent.as_deref()),
                memory.created_at,
                format!("/memories/{}.md", memory.id),
                memory.created_at,
                memory.updated_at,
            ],
        )?;
        configure(1)?;
        if let Some(source_id) = derived_from {
            let source_exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
                params![source_id],
                |row| row.get(0),
            )?;
            let linked_source = source_exists.then_some(source_id);
            // Producer is derived from the actor, never a literal. Re-parsing a
            // rendered document replaces only the rows its own producer owns, so
            // a hardcoded producer misattributes every edge written on another
            // actor's behalf — which is what migration v11 had to repair for
            // Dream. A one-shot repair does not hold: the write site has to be
            // right or the next curation pass reintroduces the same rows.
            conn.execute(
                "INSERT INTO memory_relationships (
                     id, src_memory_id, dst_memory_id, dst_ref, relation, confidence,
                     producer, source_revision, ordinal, metadata_json, created_at
                 ) VALUES (?1, ?2, ?3, ?4, 'derived_from', 'asserted',
                           ?5, 1, 0, '{}', ?6)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    memory.id,
                    linked_source,
                    source_id,
                    actor.unwrap_or("agent-memory"),
                    memory.created_at
                ],
            )?;
        }
        let (snapshot_json, content_hash) = encode_snapshot(&snapshot(conn, &memory.id)?)?;
        conn.execute(
            "INSERT INTO memory_revisions (
                 id, memory_id, revision, parent_revision, operation, actor,
                 snapshot_json, content_hash, created_at
             ) VALUES (?1, ?2, 1, NULL, ?3, ?4, ?5, ?6, ?7)",
            params![
                uuid::Uuid::new_v4().to_string(),
                memory.id,
                operation,
                actor,
                snapshot_json,
                content_hash,
                memory.updated_at,
            ],
        )?;
        crate::search::index::rebuild_memory_segments(conn, &memory.id)?;
        Ok(())
    })
}

/// Apply a semantic mutation and append history if its normalized state changes.
///
/// The callback receives the revision it is about to create, allowing typed
/// relationships to carry the correct ownership revision. A stale CAS fails
/// before the callback runs. Verification is invalidated only after a real
/// semantic change is observed.
pub fn mutate<F>(
    conn: &Connection,
    memory_id: &str,
    operation: &str,
    actor: Option<&str>,
    expected_revision: Option<i64>,
    clear_verification: bool,
    mutation: F,
) -> Result<RevisionOutcome, MemoryError>
where
    F: FnOnce(i64) -> Result<(), MemoryError>,
{
    let savepoint = savepoint_name();
    conn.execute_batch(&format!("SAVEPOINT {savepoint}"))?;
    let result = (|| {
        let revision = current_revision(conn, memory_id)?;
        if let Some(expected) = expected_revision {
            if expected != revision {
                return Err(MemoryError::RevisionConflict {
                    id: memory_id.to_string(),
                    expected,
                    actual: revision,
                });
            }
        }
        let (_, before_hash) = encode_snapshot(&snapshot(conn, memory_id)?)?;
        let next_revision = revision + 1;
        mutation(next_revision)?;
        let (_, candidate_hash) = encode_snapshot(&snapshot(conn, memory_id)?)?;
        if candidate_hash == before_hash {
            conn.execute_batch(&format!("ROLLBACK TO SAVEPOINT {savepoint}"))?;
            return Ok(RevisionOutcome {
                changed: false,
                revision,
                content_hash: before_hash,
            });
        }
        if clear_verification {
            conn.execute(
                "DELETE FROM memory_verifications WHERE memory_id = ?1",
                params![memory_id],
            )?;
        }
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE memory_concepts SET current_revision = ?1, updated_at = ?2
             WHERE memory_id = ?3",
            params![next_revision, now, memory_id],
        )?;
        let (snapshot_json, content_hash) = encode_snapshot(&snapshot(conn, memory_id)?)?;
        conn.execute(
            "INSERT INTO memory_revisions (
                 id, memory_id, revision, parent_revision, operation, actor,
                 snapshot_json, content_hash, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                uuid::Uuid::new_v4().to_string(),
                memory_id,
                next_revision,
                revision,
                operation,
                actor,
                snapshot_json,
                content_hash,
                now,
            ],
        )?;
        crate::search::index::rebuild_memory_segments(conn, memory_id)?;
        Ok(RevisionOutcome {
            changed: true,
            revision: next_revision,
            content_hash,
        })
    })();
    match result {
        Ok(outcome) => {
            conn.execute_batch(&format!("RELEASE SAVEPOINT {savepoint}"))?;
            Ok(outcome)
        }
        Err(error) => {
            let _ = conn.execute_batch(&format!(
                "ROLLBACK TO SAVEPOINT {savepoint}; RELEASE SAVEPOINT {savepoint}"
            ));
            Err(error)
        }
    }
}

/// Record an audit tombstone, then cascade the live concept and memory.
pub fn forget(
    conn: &Connection,
    memory_id: &str,
    actor: Option<&str>,
    reason: Option<&str>,
    before_delete: impl FnOnce() -> Result<(), MemoryError>,
) -> Result<bool, MemoryError> {
    in_savepoint(conn, || {
        let state = conn
            .query_row(
                "SELECT c.virtual_path, m.project, c.current_revision, r.content_hash
                 FROM memory_concepts c
                 JOIN memories m ON m.id = c.memory_id
                 JOIN memory_revisions r ON r.memory_id = c.memory_id
                                        AND r.revision = c.current_revision
                 WHERE c.memory_id = ?1",
                params![memory_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((virtual_path, project, revision, content_hash)) = state else {
            return Ok(false);
        };
        before_delete()?;
        conn.execute(
            "INSERT INTO memory_concept_tombstones (
                 id, memory_id, virtual_path, project, scope, last_revision,
                 content_hash, actor, reason, deleted_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                uuid::Uuid::new_v4().to_string(),
                memory_id,
                virtual_path,
                project,
                memory_scope(project.as_deref()),
                revision,
                content_hash,
                actor,
                reason,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        conn.execute("DELETE FROM memories WHERE id = ?1", params![memory_id])?;
        Ok(true)
    })
}

pub fn canonical_uri(memory_id: &str, project: Option<&str>) -> String {
    match project {
        Some("__global__") => format!("memory://global/{memory_id}"),
        Some(project) => format!("memory://project/{project}/{memory_id}"),
        None => format!("memory://unscoped/{memory_id}"),
    }
}

pub fn insert_relationship(
    conn: &Connection,
    src_memory_id: &str,
    dst_memory_id: Option<&str>,
    dst_ref: &str,
    relation: &str,
    source_revision: i64,
    ordinal: Option<i64>,
) -> Result<(), MemoryError> {
    insert_relationship_with_producer(
        conn,
        src_memory_id,
        dst_memory_id,
        dst_ref,
        relation,
        "agent-memory",
        source_revision,
        ordinal,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn insert_relationship_with_producer(
    conn: &Connection,
    src_memory_id: &str,
    dst_memory_id: Option<&str>,
    dst_ref: &str,
    relation: &str,
    producer: &str,
    source_revision: i64,
    ordinal: Option<i64>,
) -> Result<(), MemoryError> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_relationships (
             id, src_memory_id, dst_memory_id, dst_ref, relation, confidence,
             producer, source_revision, ordinal, metadata_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'asserted', ?6, ?7, ?8, '{}', ?9)",
        params![
            uuid::Uuid::new_v4().to_string(),
            src_memory_id,
            dst_memory_id,
            dst_ref,
            relation,
            producer,
            source_revision,
            ordinal,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::Memory, open_database, queries};

    fn memory(id: &str, content: &str) -> Memory {
        let mut memory = Memory::new(
            content.to_string(),
            Some(vec!["alpha".to_string()]),
            Some("project-a".to_string()),
            Some("agent:test".to_string()),
            None,
            Some("reference".to_string()),
        );
        memory.id = id.to_string();
        memory
    }

    #[test]
    fn insert_creates_canonical_concept_and_first_revision() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let memory = memory("memory-1", "body");
        queries::insert_memory(&conn, &memory).expect("insert");

        let concept: (String, i64, String) = conn
            .query_row(
                "SELECT concept_type, current_revision, virtual_path
                 FROM memory_concepts WHERE memory_id = ?1",
                params![memory.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("concept");
        assert_eq!(concept.0, "Agent Memory/reference");
        assert_eq!(concept.1, 1);
        assert_eq!(concept.2, "/memories/memory-1.md");
        assert_eq!(
            conn.query_row(
                "SELECT operation FROM memory_revisions WHERE memory_id = ?1",
                params![memory.id],
                |row| row.get::<_, String>(0),
            )
            .expect("revision"),
            "store"
        );
    }

    #[test]
    fn semantic_noop_rolls_back_operational_writes_and_adds_no_revision() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let memory = memory("memory-2", "same");
        queries::insert_memory(&conn, &memory).expect("insert");
        let original_updated = memory.updated_at.clone();

        let outcome = mutate(&conn, &memory.id, "noop", None, None, true, |_| {
            conn.execute(
                "UPDATE memories SET updated_at = '2099-01-01T00:00:00Z' WHERE id = ?1",
                params![memory.id],
            )?;
            Ok(())
        })
        .expect("noop");

        assert!(!outcome.changed);
        assert_eq!(outcome.revision, 1);
        assert_eq!(current_revision(&conn, &memory.id).expect("revision"), 1);
        assert_eq!(
            conn.query_row(
                "SELECT updated_at FROM memories WHERE id = ?1",
                params![memory.id],
                |row| row.get::<_, String>(0),
            )
            .expect("updated_at"),
            original_updated
        );
    }

    #[test]
    fn stale_compare_and_swap_changes_nothing() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let memory = memory("memory-3", "before");
        queries::insert_memory(&conn, &memory).expect("insert");

        let error = mutate(&conn, &memory.id, "update", None, Some(9), true, |_| {
            conn.execute(
                "UPDATE memories SET content = 'after' WHERE id = ?1",
                params![memory.id],
            )?;
            Ok(())
        })
        .expect_err("stale revision");
        assert!(matches!(
            error,
            MemoryError::RevisionConflict { actual: 1, .. }
        ));
        assert_eq!(
            queries::get_memory_by_id(&conn, &memory.id)
                .expect("memory")
                .content,
            "before"
        );
        assert_eq!(current_revision(&conn, &memory.id).expect("revision"), 1);
    }

    #[test]
    fn semantic_change_invalidates_verification_and_failure_rolls_back() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let memory = memory("memory-4", "before");
        queries::insert_memory(&conn, &memory).expect("insert");
        conn.execute(
            "INSERT INTO memory_verifications
             (id, memory_id, actor, verified_at, metadata_json)
             VALUES ('verification-1', ?1, 'human:reviewer', '2026-01-01', '{}')",
            params![memory.id],
        )
        .expect("verification");

        queries::update_content(&conn, &memory.id, "after", None, None).expect("update");
        assert_eq!(current_revision(&conn, &memory.id).expect("revision"), 2);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM memory_verifications WHERE memory_id = ?1",
                params![memory.id],
                |row| row.get::<_, i64>(0),
            )
            .expect("verification count"),
            0
        );

        let error = mutate(&conn, &memory.id, "broken", None, Some(2), true, |_| {
            conn.execute(
                "UPDATE memories SET content = 'must rollback' WHERE id = ?1",
                params![memory.id],
            )?;
            Err(MemoryError::Update("injected failure".to_string()))
        })
        .expect_err("failure");
        assert!(matches!(error, MemoryError::Update(_)));
        assert_eq!(
            queries::get_memory_by_id(&conn, &memory.id)
                .expect("memory")
                .content,
            "after"
        );
        assert_eq!(current_revision(&conn, &memory.id).expect("revision"), 2);
    }

    #[test]
    fn copy_move_supersede_and_forget_preserve_graph_and_audit_semantics() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let source = memory("memory-5", "source");
        let newer = memory("memory-6", "newer");
        queries::insert_memory(&conn, &source).expect("source");
        queries::insert_memory(&conn, &newer).expect("newer");

        let copy_id =
            queries::copy_memory_by_id(&conn, &source.id, Some("project-b")).expect("copy");
        assert_eq!(
            conn.query_row(
                "SELECT relation FROM memory_relationships
                 WHERE src_memory_id = ?1 AND dst_memory_id = ?2",
                params![copy_id, source.id],
                |row| row.get::<_, String>(0),
            )
            .expect("derived edge"),
            "derived_from"
        );

        assert!(queries::move_memory_by_id(&conn, &source.id, Some("project-c")).expect("move"));
        queries::mark_superseded(&conn, &source.id, &newer.id).expect("supersede");
        let relations: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT relation FROM memory_relationships
                     WHERE src_memory_id = ?1 ORDER BY relation",
                )
                .expect("prepare");
            stmt.query_map(params![source.id], |row| row.get(0))
                .expect("query")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect")
        };
        assert_eq!(relations, vec!["aliases"]);
        assert_eq!(
            conn.query_row(
                "SELECT relation FROM memory_relationships
                 WHERE src_memory_id = ?1 AND dst_memory_id = ?2",
                params![newer.id, source.id],
                |row| row.get::<_, String>(0),
            )
            .expect("supersession edge"),
            "supersedes"
        );

        assert!(queries::delete_memory(&conn, &source.id).expect("forget"));
        assert_eq!(
            conn.query_row(
                "SELECT last_revision FROM memory_concept_tombstones WHERE memory_id = ?1",
                params![source.id],
                |row| row.get::<_, i64>(0),
            )
            .expect("tombstone"),
            3
        );
    }

    #[test]
    fn persistence_is_safe_inside_an_existing_transaction() {
        let mut conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let tx = conn.transaction().expect("outer transaction");
        let memory = memory("memory-7", "nested");
        queries::insert_memory(&tx, &memory).expect("nested insert");
        tx.rollback().expect("outer rollback");

        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row
                .get::<_, i64>(0))
                .expect("count"),
            0
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM memory_revisions", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count"),
            0
        );
    }
}
