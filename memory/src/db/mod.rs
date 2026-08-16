pub mod models;
pub mod queries;

use rusqlite::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

use crate::error::MemoryError;

pub fn open_database(db_path: &Path) -> Result<Connection, MemoryError> {
    if db_path != Path::new(":memory:") {
        if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
    }

    let conn = Connection::open(db_path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    // WAL mode for better concurrent read performance
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;

    run_migrations(&conn)?;

    Ok(conn)
}

pub(crate) fn run_migrations(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch("SAVEPOINT agent_memory_migrations")?;
    match run_migrations_inner(conn) {
        Ok(()) => {
            conn.execute_batch("RELEASE SAVEPOINT agent_memory_migrations")?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute_batch(
                "ROLLBACK TO SAVEPOINT agent_memory_migrations;
                 RELEASE SAVEPOINT agent_memory_migrations",
            );
            Err(error)
        }
    }
}

fn run_migrations_inner(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY
        );",
    )?;

    let version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if version < 1 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                tags TEXT,
                project TEXT,
                agent TEXT,
                source_file TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                access_count INTEGER DEFAULT 0,
                embedding BLOB,
                memory_type TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_memories_project ON memories(project);
            CREATE INDEX IF NOT EXISTS idx_memories_agent ON memories(agent);
            CREATE INDEX IF NOT EXISTS idx_memories_memory_type ON memories(memory_type);
            CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON memories(updated_at);

            INSERT OR IGNORE INTO schema_version (version) VALUES (1);",
        )?;
    }

    if version < 2 {
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                content,
                content='memories',
                content_rowid='rowid',
                tokenize='porter unicode61'
            );

            CREATE TRIGGER IF NOT EXISTS memories_fts_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
            END;

            CREATE TRIGGER IF NOT EXISTS memories_fts_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content)
                    VALUES('delete', old.rowid, old.content);
            END;

            CREATE TRIGGER IF NOT EXISTS memories_fts_au AFTER UPDATE OF content ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content)
                    VALUES('delete', old.rowid, old.content);
                INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
            END;

            -- Populate FTS index from existing data
            INSERT INTO memories_fts(memories_fts) VALUES('rebuild');

            INSERT OR IGNORE INTO schema_version (version) VALUES (2);",
        )?;
    }

    if version < 3 {
        // Schema v3 — columns added to support `memory-dream` (Release 2).
        //
        // All four columns are nullable so pre-dream rows remain valid:
        //   * content_raw        — original verbatim text the user stored.
        //     When dream condenses a memory, the short form replaces `content`
        //     and the original moves here so nothing is lost.
        //   * superseded_by      — UUID of a newer memory that subsumes this
        //     one (dedup). Default reads filter `superseded_by IS NULL` so
        //     obsoleted rows stay in the DB for audit but don't surface in
        //     search, context, or list.
        //   * condenser_version  — stamp identifying the prompt/model combo
        //     that produced the current `content`. Lets a future dream pass
        //     detect stale condensations and re-run.
        //   * embedding_model    — name of the embedder used to compute
        //     `embedding`. Dream uses this so it only dedups rows that share
        //     a vector space.
        //
        // The FTS triggers are dropped and recreated to index
        // `content || ' ' || COALESCE(content_raw, '')` — otherwise terms the
        // condenser elided stop surfacing via lexical recall. The FTS table
        // itself stays single-column; the triggers concatenate before insert.
        conn.execute_batch(
            "ALTER TABLE memories ADD COLUMN content_raw TEXT;
             ALTER TABLE memories ADD COLUMN superseded_by TEXT;
             ALTER TABLE memories ADD COLUMN condenser_version TEXT;
             ALTER TABLE memories ADD COLUMN embedding_model TEXT;

             DROP TRIGGER IF EXISTS memories_fts_ai;
             DROP TRIGGER IF EXISTS memories_fts_ad;
             DROP TRIGGER IF EXISTS memories_fts_au;

             CREATE TRIGGER memories_fts_ai AFTER INSERT ON memories BEGIN
                 INSERT INTO memories_fts(rowid, content)
                     VALUES (new.rowid, new.content || ' ' || COALESCE(new.content_raw, ''));
             END;

             CREATE TRIGGER memories_fts_ad AFTER DELETE ON memories BEGIN
                 INSERT INTO memories_fts(memories_fts, rowid, content)
                     VALUES('delete', old.rowid, old.content || ' ' || COALESCE(old.content_raw, ''));
             END;

             CREATE TRIGGER memories_fts_au AFTER UPDATE OF content, content_raw ON memories BEGIN
                 INSERT INTO memories_fts(memories_fts, rowid, content)
                     VALUES('delete', old.rowid, old.content || ' ' || COALESCE(old.content_raw, ''));
                 INSERT INTO memories_fts(rowid, content)
                     VALUES (new.rowid, new.content || ' ' || COALESCE(new.content_raw, ''));
             END;

             -- Rebuild so rows inserted pre-v3 reindex under the new trigger body.
             INSERT INTO memories_fts(memories_fts) VALUES('rebuild');

             CREATE INDEX IF NOT EXISTS idx_memories_superseded_by
                 ON memories(superseded_by);

             INSERT OR IGNORE INTO schema_version (version) VALUES (3);",
        )?;
    }

    if version < 4 {
        // Schema v4 — per-project dream state (Release 2.3).
        //
        // The agentic dream pass runs incrementally. Each project's
        // `last_dream_at` records the wall-clock instant we last curated
        // memories for that project, so the next pass can pull only rows
        // with `updated_at > last_dream_at` OR a stale `condenser_version`.
        //
        // Absent row = "never dreamed"; dream treats the timestamp as epoch
        // on the first pass. Stored as RFC3339 so the column survives
        // timezone migrations. No backfill needed — pre-v4 DBs simply have
        // no rows, so every project re-walks once on the first upgraded
        // dream pass (expected behavior).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS project_state (
                 project TEXT PRIMARY KEY,
                 last_dream_at TEXT NOT NULL
             );
             INSERT OR IGNORE INTO schema_version (version) VALUES (4);",
        )?;
    }

    if version < 5 {
        // Schema v5 — per-project WorkingContext handoff state.
        //
        // This is intentionally separate from durable memories: it is live
        // handoff state for the current project, not ranked knowledge for
        // search/dream. One row means an active WorkingContext; no row means
        // no handoff has been set. `clear` deletes the row.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS project_working_context (
                 project TEXT PRIMARY KEY NOT NULL
                     CHECK (project <> '__global__'),
                 content TEXT NOT NULL
                     CHECK (LENGTH(content) <= 65536),
                 version INTEGER NOT NULL DEFAULT 1,
                 updated_at TEXT NOT NULL
             );
             INSERT OR IGNORE INTO schema_version (version) VALUES (5);",
        )?;
    }

    if version < 6 {
        // Schema v6 — gateway exchange metadata for durable memories.
        //
        // Durable memory rows remain canonical for local recall. These side
        // tables only record mapping/cursor state needed by `memory push` and
        // `memory pull`, so sync bookkeeping does not mutate memory content or
        // bump `memories.updated_at`.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_gateway_sync (
                 local_memory_id TEXT PRIMARY KEY NOT NULL,
                 project TEXT NOT NULL,
                 gateway_memory_id TEXT NOT NULL,
                 last_seen_server_revision INTEGER NOT NULL,
                 last_pushed_content_hash TEXT,
                 last_pulled_content_hash TEXT,
                 sync_state TEXT NOT NULL,
                 tombstone_deleted INTEGER NOT NULL DEFAULT 0
                     CHECK (tombstone_deleted IN (0, 1)),
                 tombstone_at TEXT,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 FOREIGN KEY(local_memory_id) REFERENCES memories(id) ON DELETE CASCADE,
                 UNIQUE(project, gateway_memory_id)
             );
             CREATE INDEX IF NOT EXISTS idx_memory_gateway_sync_project
                 ON memory_gateway_sync(project);
             CREATE INDEX IF NOT EXISTS idx_memory_gateway_sync_gateway_id
                 ON memory_gateway_sync(project, gateway_memory_id);

             CREATE TABLE IF NOT EXISTS project_gateway_sync_state (
                 project TEXT PRIMARY KEY NOT NULL,
                 last_pull_server_revision INTEGER,
                 last_pull_cursor TEXT,
                 updated_at TEXT NOT NULL
             );

             INSERT OR IGNORE INTO schema_version (version) VALUES (6);",
        )?;
    }

    if version < 7 {
        // Schema v7 — allow gateway sync metadata for global-scope durable
        // memories. WorkingContext remains project-only in its own table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_gateway_sync_v7 (
                 local_memory_id TEXT PRIMARY KEY NOT NULL,
                 project TEXT NOT NULL,
                 gateway_memory_id TEXT NOT NULL,
                 last_seen_server_revision INTEGER NOT NULL,
                 last_pushed_content_hash TEXT,
                 last_pulled_content_hash TEXT,
                 sync_state TEXT NOT NULL,
                 tombstone_deleted INTEGER NOT NULL DEFAULT 0
                     CHECK (tombstone_deleted IN (0, 1)),
                 tombstone_at TEXT,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 FOREIGN KEY(local_memory_id) REFERENCES memories(id) ON DELETE CASCADE,
                 UNIQUE(project, gateway_memory_id)
             );
             INSERT OR IGNORE INTO memory_gateway_sync_v7 (
                 local_memory_id, project, gateway_memory_id,
                 last_seen_server_revision, last_pushed_content_hash,
                 last_pulled_content_hash, sync_state, tombstone_deleted,
                 tombstone_at, created_at, updated_at
             )
             SELECT local_memory_id, project, gateway_memory_id,
                    last_seen_server_revision, last_pushed_content_hash,
                    last_pulled_content_hash, sync_state, tombstone_deleted,
                    tombstone_at, created_at, updated_at
             FROM memory_gateway_sync;
             DROP TABLE memory_gateway_sync;
             ALTER TABLE memory_gateway_sync_v7 RENAME TO memory_gateway_sync;
             CREATE INDEX IF NOT EXISTS idx_memory_gateway_sync_project
                 ON memory_gateway_sync(project);
             CREATE INDEX IF NOT EXISTS idx_memory_gateway_sync_gateway_id
                 ON memory_gateway_sync(project, gateway_memory_id);

             CREATE TABLE IF NOT EXISTS project_gateway_sync_state_v7 (
                 project TEXT PRIMARY KEY NOT NULL,
                 last_pull_server_revision INTEGER,
                 last_pull_cursor TEXT,
                 updated_at TEXT NOT NULL
             );
             INSERT OR IGNORE INTO project_gateway_sync_state_v7 (
                 project, last_pull_server_revision, last_pull_cursor, updated_at
             )
             SELECT project, last_pull_server_revision, last_pull_cursor, updated_at
             FROM project_gateway_sync_state;
             DROP TABLE project_gateway_sync_state;
             ALTER TABLE project_gateway_sync_state_v7 RENAME TO project_gateway_sync_state;

             INSERT OR IGNORE INTO schema_version (version) VALUES (7);",
        )?;
    }

    if version < 8 {
        // Schema v8 — local delete queue for gateway tombstones.
        //
        // `memory_gateway_sync` intentionally cascades with `memories`, which
        // is right for normal metadata but wrong for "deleted locally, still
        // needs gateway tombstone" reconciliation. This queue is not
        // foreign-keyed to `memories` so `memory push --all` can still discover
        // gateway records that vanished locally before a successful push.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_gateway_delete_queue (
                 local_memory_id TEXT PRIMARY KEY NOT NULL,
                 project TEXT NOT NULL,
                 gateway_memory_id TEXT NOT NULL,
                 last_seen_server_revision INTEGER NOT NULL,
                 last_pushed_content_hash TEXT,
                 last_pulled_content_hash TEXT,
                 tombstone_at TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 UNIQUE(project, gateway_memory_id)
             );
             CREATE INDEX IF NOT EXISTS idx_memory_gateway_delete_queue_project
                 ON memory_gateway_delete_queue(project);
             CREATE INDEX IF NOT EXISTS idx_memory_gateway_delete_queue_gateway_id
                 ON memory_gateway_delete_queue(project, gateway_memory_id);

             INSERT OR IGNORE INTO schema_version (version) VALUES (8);",
        )?;
    }

    if version < 9 {
        migrate_v9_okf_native_concepts(conn)?;
    }
    if version < 10 {
        migrate_v10_okf_search_segments(conn)?;
    }
    if version < 11 {
        migrate_v11_dream_relationship_provenance(conn)?;
    }
    if version < 12 {
        migrate_v12_namespaced_dream_operations(conn)?;
    }

    Ok(())
}

/// Bring historical curation revisions onto the namespaced operation
/// vocabulary.
///
/// `dream_merge` was already namespaced but `condense`, `extract` and
/// `supersede` were not, so an audit filter written against the documented
/// vocabulary silently missed most of what Dream had done. The operation is a
/// label: it is not part of the semantic snapshot or content hash, so
/// relabelling does not alter any recorded state.
///
/// Scoped to revisions actually written by the curator, so an unrelated writer
/// that used a bare verb keeps its own history.
fn migrate_v12_namespaced_dream_operations(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(
        "UPDATE memory_revisions
         SET operation = 'dream_' || operation
         WHERE actor = 'memory-dream'
           AND operation IN ('condense', 'extract', 'supersede');
         INSERT OR IGNORE INTO schema_version (version) VALUES (12);",
    )?;
    Ok(())
}

fn migrate_v11_dream_relationship_provenance(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(
        "DELETE FROM memory_relationships
         WHERE id IN (
             SELECT incorrect.id
             FROM memory_relationships AS incorrect
             JOIN memory_revisions AS revision
               ON revision.memory_id = incorrect.src_memory_id
              AND revision.revision = incorrect.source_revision
              AND revision.actor = 'memory-dream'
             JOIN memory_relationships AS correct
               ON correct.src_memory_id = incorrect.src_memory_id
              AND correct.producer = 'memory-dream'
              AND correct.source_revision = incorrect.source_revision
              AND correct.relation = incorrect.relation
              AND correct.dst_ref = incorrect.dst_ref
              AND COALESCE(correct.ordinal, -1) = COALESCE(incorrect.ordinal, -1)
             WHERE incorrect.producer = 'agent-memory'
         );
         UPDATE memory_relationships AS relationship
         SET producer = 'memory-dream'
         WHERE producer = 'agent-memory'
           AND EXISTS (
               SELECT 1
               FROM memory_revisions AS revision
               WHERE revision.memory_id = relationship.src_memory_id
                 AND revision.revision = relationship.source_revision
                 AND revision.actor = 'memory-dream'
           );
         INSERT OR IGNORE INTO schema_version (version) VALUES (11);",
    )?;
    Ok(())
}

fn migrate_v10_okf_search_segments(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS memory_segments_fts USING fts5(
                 memory_id UNINDEXED,
                 segment_no UNINDEXED,
                 heading_path UNINDEXED,
                 searchable,
                 tokenize='porter unicode61'
             );
             CREATE TRIGGER IF NOT EXISTS memory_segments_fts_ad
             AFTER DELETE ON memories BEGIN
                 DELETE FROM memory_segments_fts WHERE memory_id = old.id;
             END;",
    )?;
    crate::search::index::rebuild_all_segments(conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO schema_version (version) VALUES (10)",
        [],
    )?;
    Ok(())
}

#[derive(Debug)]
struct LegacyConceptRow {
    id: String,
    content: String,
    tags_json: Option<String>,
    project: Option<String>,
    agent: Option<String>,
    source_file: Option<String>,
    created_at: String,
    updated_at: String,
    memory_type: Option<String>,
    content_raw: Option<String>,
    superseded_by: Option<String>,
}

#[derive(Serialize)]
struct MigratedConceptSnapshot<'a> {
    body: &'a str,
    content_raw: &'a Option<String>,
    tags: serde_json::Value,
    project: &'a Option<String>,
    agent: &'a Option<String>,
    source_file: &'a Option<String>,
    memory_type: &'a str,
    superseded_by: &'a Option<String>,
    okf: MigratedOkfSnapshot<'a>,
}

#[derive(Serialize)]
struct MigratedOkfSnapshot<'a> {
    #[serde(rename = "type")]
    concept_type: &'a str,
    status: &'static str,
    #[serde(rename = "x-agent-memory")]
    agent_memory: MigratedAgentMemorySnapshot<'a>,
}

#[derive(Serialize)]
struct MigratedAgentMemorySnapshot<'a> {
    id: &'a str,
    revision: i64,
    memory_type: &'a str,
    project: &'a Option<String>,
    scope: &'static str,
}

fn migrate_v9_okf_native_concepts(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_concepts (
                 memory_id TEXT PRIMARY KEY NOT NULL,
                 concept_type TEXT NOT NULL CHECK (TRIM(concept_type) <> ''),
                 title TEXT,
                 description TEXT,
                 resource TEXT,
                 status TEXT NOT NULL DEFAULT 'stable'
                     CHECK (status IN ('draft', 'stable', 'deprecated')),
                 stale_after TEXT,
                 generated_by TEXT,
                 generated_at TEXT,
                 extensions_json TEXT NOT NULL DEFAULT '{}',
                 raw_frontmatter TEXT,
                 current_revision INTEGER NOT NULL DEFAULT 1
                     CHECK (current_revision > 0),
                 virtual_path TEXT NOT NULL UNIQUE,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 FOREIGN KEY(memory_id) REFERENCES memories(id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS idx_memory_concepts_type
                 ON memory_concepts(concept_type);
             CREATE INDEX IF NOT EXISTS idx_memory_concepts_status
                 ON memory_concepts(status);
             CREATE INDEX IF NOT EXISTS idx_memory_concepts_stale_after
                 ON memory_concepts(stale_after);

             CREATE TABLE IF NOT EXISTS memory_revisions (
                 id TEXT PRIMARY KEY NOT NULL,
                 memory_id TEXT NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 parent_revision INTEGER,
                 operation TEXT NOT NULL CHECK (TRIM(operation) <> ''),
                 actor TEXT,
                 snapshot_json TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 FOREIGN KEY(memory_id) REFERENCES memories(id) ON DELETE CASCADE,
                 UNIQUE(memory_id, revision)
             );
             CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_created
                 ON memory_revisions(memory_id, created_at DESC);
             CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_hash
                 ON memory_revisions(memory_id, content_hash);

             CREATE TABLE IF NOT EXISTS memory_sources (
                 memory_id TEXT NOT NULL,
                 source_key TEXT NOT NULL CHECK (TRIM(source_key) <> ''),
                 ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
                 resource TEXT NOT NULL CHECK (TRIM(resource) <> ''),
                 title TEXT,
                 author TEXT,
                 usage_count INTEGER CHECK (usage_count IS NULL OR usage_count >= 0),
                 usage_window_from TEXT,
                 usage_window_to TEXT,
                 last_modified TEXT,
                 metadata_json TEXT NOT NULL DEFAULT '{}',
                 FOREIGN KEY(memory_id) REFERENCES memories(id) ON DELETE CASCADE,
                 PRIMARY KEY(memory_id, source_key),
                 UNIQUE(memory_id, ordinal)
             );

             CREATE TABLE IF NOT EXISTS memory_verifications (
                 id TEXT PRIMARY KEY NOT NULL,
                 memory_id TEXT NOT NULL,
                 actor TEXT NOT NULL CHECK (TRIM(actor) <> ''),
                 verified_at TEXT NOT NULL CHECK (TRIM(verified_at) <> ''),
                 verification_kind TEXT,
                 metadata_json TEXT NOT NULL DEFAULT '{}',
                 FOREIGN KEY(memory_id) REFERENCES memories(id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS idx_memory_verifications_memory
                 ON memory_verifications(memory_id, verified_at DESC);

             CREATE TABLE IF NOT EXISTS memory_relationships (
                 id TEXT PRIMARY KEY NOT NULL,
                 src_memory_id TEXT NOT NULL,
                 dst_memory_id TEXT,
                 dst_ref TEXT NOT NULL CHECK (TRIM(dst_ref) <> ''),
                 relation TEXT NOT NULL CHECK (TRIM(relation) <> ''),
                 confidence TEXT NOT NULL CHECK (TRIM(confidence) <> ''),
                 producer TEXT NOT NULL CHECK (TRIM(producer) <> ''),
                 source_revision INTEGER NOT NULL CHECK (source_revision > 0),
                 ordinal INTEGER,
                 metadata_json TEXT NOT NULL DEFAULT '{}',
                 created_at TEXT NOT NULL,
                 FOREIGN KEY(src_memory_id) REFERENCES memories(id) ON DELETE CASCADE,
                 FOREIGN KEY(dst_memory_id) REFERENCES memories(id) ON DELETE SET NULL
             );
             CREATE INDEX IF NOT EXISTS idx_memory_relationships_src
                 ON memory_relationships(src_memory_id, relation);
             CREATE INDEX IF NOT EXISTS idx_memory_relationships_dst
                 ON memory_relationships(dst_memory_id, relation);
             CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_relationships_owned_unique
                 ON memory_relationships(
                     src_memory_id, producer, source_revision, relation, dst_ref,
                     COALESCE(ordinal, -1)
                 );

             CREATE TABLE IF NOT EXISTS memory_concept_tombstones (
                 id TEXT PRIMARY KEY NOT NULL,
                 memory_id TEXT NOT NULL,
                 virtual_path TEXT NOT NULL,
                 project TEXT,
                 scope TEXT NOT NULL,
                 last_revision INTEGER NOT NULL,
                 content_hash TEXT NOT NULL,
                 actor TEXT,
                 reason TEXT,
                 deleted_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_memory_concept_tombstones_memory
                 ON memory_concept_tombstones(memory_id, deleted_at DESC);",
    )?;

    let legacy_rows = {
        let mut stmt = conn.prepare(
            "SELECT id, content, tags, project, agent, source_file,
                        created_at, updated_at, memory_type, content_raw, superseded_by
                 FROM memories
                 WHERE id NOT IN (SELECT memory_id FROM memory_concepts)
                 ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(LegacyConceptRow {
                id: row.get(0)?,
                content: row.get(1)?,
                tags_json: row.get(2)?,
                project: row.get(3)?,
                agent: row.get(4)?,
                source_file: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                memory_type: row.get(8)?,
                content_raw: row.get(9)?,
                superseded_by: row.get(10)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    for row in &legacy_rows {
        let memory_type = row
            .memory_type
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("user");
        let concept_type = format!("Agent Memory/{memory_type}");
        let scope = match row.project.as_deref() {
            Some("__global__") => "global",
            Some(_) => "project",
            None => "unscoped",
        };
        let tags = row
            .tags_json
            .as_deref()
            .map(|raw| {
                serde_json::from_str(raw)
                    .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
            })
            .unwrap_or(serde_json::Value::Null);
        let snapshot = MigratedConceptSnapshot {
            body: &row.content,
            content_raw: &row.content_raw,
            tags,
            project: &row.project,
            agent: &row.agent,
            source_file: &row.source_file,
            memory_type,
            superseded_by: &row.superseded_by,
            okf: MigratedOkfSnapshot {
                concept_type: &concept_type,
                status: "stable",
                agent_memory: MigratedAgentMemorySnapshot {
                    id: &row.id,
                    revision: 1,
                    memory_type,
                    project: &row.project,
                    scope,
                },
            },
        };
        let snapshot_json = serde_json::to_string(&snapshot)?;
        let mut hasher = Sha256::new();
        hasher.update(snapshot_json.as_bytes());
        let content_hash = format!("{:x}", hasher.finalize());

        conn.execute(
            "INSERT INTO memory_concepts (
                     memory_id, concept_type, status, extensions_json,
                     current_revision, virtual_path, created_at, updated_at
                 ) VALUES (?1, ?2, 'stable', '{}', 1, ?3, ?4, ?5)",
            params![
                row.id,
                concept_type,
                format!("/memories/{}.md", row.id),
                row.created_at,
                row.updated_at,
            ],
        )?;
        conn.execute(
            "INSERT INTO memory_revisions (
                     id, memory_id, revision, parent_revision, operation,
                     actor, snapshot_json, content_hash, created_at
                 ) VALUES (?1, ?2, 1, NULL, 'migrate', NULL, ?3, ?4, ?5)",
            params![
                uuid::Uuid::new_v4().to_string(),
                row.id,
                snapshot_json,
                content_hash,
                row.updated_at,
            ],
        )?;
    }

    let memory_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
    let concept_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM memory_concepts", [], |row| row.get(0))?;
    let revision_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_revisions WHERE revision = 1",
        [],
        |row| row.get(0),
    )?;
    if memory_count != concept_count || memory_count != revision_count {
        return Err(MemoryError::Config(format!(
            "OKF-native migration count mismatch: memories={memory_count}, \
                 concepts={concept_count}, revision_1={revision_count}"
        )));
    }

    conn.execute(
        "INSERT OR IGNORE INTO schema_version (version) VALUES (9)",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// Simulates a DB created at schema v2 (pre-Release-2), then runs
    /// `run_migrations` against it to confirm the v3 upgrade applies cleanly,
    /// new columns read as NULL on pre-existing rows, and the FTS index is
    /// rebuilt with the concatenated-content trigger body.
    #[test]
    fn v2_database_upgrades_to_v3_preserving_existing_rows() {
        let conn = Connection::open_in_memory().expect("open in-memory db");

        // Hand-construct a v2 schema (what the DB looked like before Release 2).
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             CREATE TABLE memories (
                 id TEXT PRIMARY KEY,
                 content TEXT NOT NULL,
                 tags TEXT,
                 project TEXT,
                 agent TEXT,
                 source_file TEXT,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 access_count INTEGER DEFAULT 0,
                 embedding BLOB,
                 memory_type TEXT
             );
             CREATE VIRTUAL TABLE memories_fts USING fts5(
                 content, content='memories', content_rowid='rowid',
                 tokenize='porter unicode61'
             );
             CREATE TRIGGER memories_fts_ai AFTER INSERT ON memories BEGIN
                 INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
             END;
             CREATE TRIGGER memories_fts_ad AFTER DELETE ON memories BEGIN
                 INSERT INTO memories_fts(memories_fts, rowid, content)
                     VALUES('delete', old.rowid, old.content);
             END;
             CREATE TRIGGER memories_fts_au AFTER UPDATE OF content ON memories BEGIN
                 INSERT INTO memories_fts(memories_fts, rowid, content)
                     VALUES('delete', old.rowid, old.content);
                 INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
             END;
             INSERT INTO schema_version (version) VALUES (1);
             INSERT INTO schema_version (version) VALUES (2);
             INSERT INTO memories (id, content, created_at, updated_at)
                 VALUES ('legacy-row', 'existing v2 content', '2026-01-01', '2026-01-01');",
        )
        .expect("seed v2 db");

        // Apply migrations — should run every step after v2 in sequence.
        run_migrations(&conn).expect("migrate to latest");

        // Schema version advanced to latest.
        let max_v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("query schema_version");
        assert_eq!(max_v, 12);

        // New columns are present and NULL on the pre-existing row.
        let (raw, sup, cond, emb): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT content_raw, superseded_by, condenser_version, embedding_model
                 FROM memories WHERE id = 'legacy-row'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("select new columns");
        assert!(raw.is_none());
        assert!(sup.is_none());
        assert!(cond.is_none());
        assert!(emb.is_none());

        // FTS rebuild ran — the pre-existing content should still be searchable.
        let hit_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'existing'",
                [],
                |row| row.get(0),
            )
            .expect("fts query");
        assert_eq!(hit_count, 1);
    }

    /// Fresh DB path: no prior schema_version rows, run_migrations should
    /// apply every step (1 through latest) and leave an empty but well-formed DB.
    #[test]
    fn fresh_database_applies_all_migrations() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        run_migrations(&conn).expect("migrate fresh");
        let max_v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("query schema_version");
        assert_eq!(max_v, 12);
    }

    #[test]
    fn open_database_creates_parent_dirs_and_sets_busy_timeout() {
        let root = std::env::temp_dir().join(format!("agent-memory-db-{}", uuid::Uuid::new_v4()));
        let db_path = root.join("nested").join("memory.db");

        let conn = open_database(&db_path).expect("open database");

        assert!(db_path.exists());
        let timeout_ms: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("read busy timeout");
        assert_eq!(timeout_ms, 5000);

        drop(conn);
        let _ = std::fs::remove_dir_all(root);
    }

    /// v4 migration from a v3 fixture DB must create the `project_state`
    /// table without disturbing existing memory rows. Mirrors the v2→v3 test
    /// so we exercise the upgrade path for each release independently.
    #[test]
    fn v3_database_upgrades_to_v4_creating_project_state() {
        let conn = Connection::open_in_memory().expect("open in-memory db");

        // Hand-construct a v3 schema + a memory row so we can verify the v4
        // migration adds `project_state` without touching existing data.
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             CREATE TABLE memories (
                 id TEXT PRIMARY KEY, content TEXT NOT NULL,
                 tags TEXT, project TEXT, agent TEXT, source_file TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 access_count INTEGER DEFAULT 0, embedding BLOB, memory_type TEXT,
                 content_raw TEXT, superseded_by TEXT,
                 condenser_version TEXT, embedding_model TEXT
             );
             CREATE VIRTUAL TABLE memories_fts USING fts5(
                 content, content='memories', content_rowid='rowid',
                 tokenize='porter unicode61'
             );
             INSERT INTO schema_version (version) VALUES (1);
             INSERT INTO schema_version (version) VALUES (2);
             INSERT INTO schema_version (version) VALUES (3);
             INSERT INTO memories (id, content, project, created_at, updated_at)
                 VALUES ('existing-row', 'body', 'agent-memory', '2026-01-01', '2026-01-01');",
        )
        .expect("seed v3 db");

        run_migrations(&conn).expect("migrate to latest");

        // project_state now exists and is empty (no backfill).
        let row_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_state", [], |row| row.get(0))
            .expect("query project_state");
        assert_eq!(row_count, 0);

        // Schema version advanced to latest.
        let max_v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("query schema_version");
        assert_eq!(max_v, 12);

        // Existing memory row survived untouched.
        let existing: String = conn
            .query_row(
                "SELECT content FROM memories WHERE id = 'existing-row'",
                [],
                |row| row.get(0),
            )
            .expect("select existing row");
        assert_eq!(existing, "body");
    }

    /// v5+ migrations from a v4 fixture DB must create the WorkingContext and
    /// gateway sync tables without disturbing `project_state` or existing
    /// memory rows.
    #[test]
    fn v4_database_upgrades_to_v5_creating_working_context() {
        let conn = Connection::open_in_memory().expect("open in-memory db");

        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             CREATE TABLE memories (
                 id TEXT PRIMARY KEY, content TEXT NOT NULL,
                 tags TEXT, project TEXT, agent TEXT, source_file TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 access_count INTEGER DEFAULT 0, embedding BLOB, memory_type TEXT,
                 content_raw TEXT, superseded_by TEXT,
                 condenser_version TEXT, embedding_model TEXT
             );
             CREATE VIRTUAL TABLE memories_fts USING fts5(
                 content, content='memories', content_rowid='rowid',
                 tokenize='porter unicode61'
             );
             CREATE TABLE project_state (
                 project TEXT PRIMARY KEY,
                 last_dream_at TEXT NOT NULL
             );
             INSERT INTO schema_version (version) VALUES (1);
             INSERT INTO schema_version (version) VALUES (2);
             INSERT INTO schema_version (version) VALUES (3);
             INSERT INTO schema_version (version) VALUES (4);
             INSERT INTO memories (id, content, project, created_at, updated_at)
                 VALUES ('existing-row', 'body', 'agent-memory', '2026-01-01', '2026-01-01');
             INSERT INTO project_state (project, last_dream_at)
                 VALUES ('agent-memory', '2026-04-23T00:00:00Z');",
        )
        .expect("seed v4 db");

        run_migrations(&conn).expect("migrate to latest");

        let working_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_working_context", [], |row| {
                row.get(0)
            })
            .expect("query project_working_context");
        assert_eq!(working_count, 0);

        let gateway_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_gateway_sync", [], |row| {
                row.get(0)
            })
            .expect("query memory_gateway_sync");
        assert_eq!(gateway_count, 0);

        let dream_ts: String = conn
            .query_row(
                "SELECT last_dream_at FROM project_state WHERE project = 'agent-memory'",
                [],
                |row| row.get(0),
            )
            .expect("query project_state");
        assert_eq!(dream_ts, "2026-04-23T00:00:00Z");

        let max_v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("query schema_version");
        assert_eq!(max_v, 12);

        let existing: String = conn
            .query_row(
                "SELECT content FROM memories WHERE id = 'existing-row'",
                [],
                |row| row.get(0),
            )
            .expect("select existing row");
        assert_eq!(existing, "body");
    }

    /// v6+ migrations from a v5 fixture DB must add gateway exchange
    /// metadata tables without disturbing WorkingContext or durable memory rows.
    #[test]
    fn v5_database_upgrades_to_latest_creating_gateway_sync_tables() {
        let conn = Connection::open_in_memory().expect("open in-memory db");

        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             CREATE TABLE memories (
                 id TEXT PRIMARY KEY, content TEXT NOT NULL,
                 tags TEXT, project TEXT, agent TEXT, source_file TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 access_count INTEGER DEFAULT 0, embedding BLOB, memory_type TEXT,
                 content_raw TEXT, superseded_by TEXT,
                 condenser_version TEXT, embedding_model TEXT
             );
             CREATE VIRTUAL TABLE memories_fts USING fts5(
                 content, content='memories', content_rowid='rowid',
                 tokenize='porter unicode61'
             );
             CREATE TABLE project_state (
                 project TEXT PRIMARY KEY,
                 last_dream_at TEXT NOT NULL
             );
             CREATE TABLE project_working_context (
                 project TEXT PRIMARY KEY NOT NULL
                     CHECK (project <> '__global__'),
                 content TEXT NOT NULL
                     CHECK (LENGTH(content) <= 65536),
                 version INTEGER NOT NULL DEFAULT 1,
                 updated_at TEXT NOT NULL
             );
             INSERT INTO schema_version (version) VALUES (1);
             INSERT INTO schema_version (version) VALUES (2);
             INSERT INTO schema_version (version) VALUES (3);
             INSERT INTO schema_version (version) VALUES (4);
             INSERT INTO schema_version (version) VALUES (5);
             INSERT INTO memories (id, content, project, created_at, updated_at)
                 VALUES ('existing-row', 'body', 'agent-memory', '2026-01-01', '2026-01-01');
             INSERT INTO project_working_context (project, content, version, updated_at)
                 VALUES ('agent-memory', 'handoff', 1, '2026-04-23T00:00:00Z');",
        )
        .expect("seed v5 db");

        run_migrations(&conn).expect("migrate to latest");

        let max_v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("query schema_version");
        assert_eq!(max_v, 12);

        let sync_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_gateway_sync", [], |row| {
                row.get(0)
            })
            .expect("query memory_gateway_sync");
        assert_eq!(sync_count, 0);

        let project_sync_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM project_gateway_sync_state",
                [],
                |row| row.get(0),
            )
            .expect("query project_gateway_sync_state");
        assert_eq!(project_sync_count, 0);

        let delete_queue_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_gateway_delete_queue",
                [],
                |row| row.get(0),
            )
            .expect("query memory_gateway_delete_queue");
        assert_eq!(delete_queue_count, 0);

        let handoff: String = conn
            .query_row(
                "SELECT content FROM project_working_context WHERE project = 'agent-memory'",
                [],
                |row| row.get(0),
            )
            .expect("query working context");
        assert_eq!(handoff, "handoff");

        let existing: String = conn
            .query_row(
                "SELECT content FROM memories WHERE id = 'existing-row'",
                [],
                |row| row.get(0),
            )
            .expect("select existing row");
        assert_eq!(existing, "body");
    }

    #[test]
    fn v6_database_upgrades_to_latest_allowing_global_gateway_sync() {
        let conn = Connection::open_in_memory().expect("open in-memory db");

        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
             CREATE TABLE memories (
                 id TEXT PRIMARY KEY, content TEXT NOT NULL,
                 tags TEXT, project TEXT, agent TEXT, source_file TEXT,
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 access_count INTEGER DEFAULT 0, embedding BLOB, memory_type TEXT,
                 content_raw TEXT, superseded_by TEXT,
                 condenser_version TEXT, embedding_model TEXT
             );
             CREATE TABLE memory_gateway_sync (
                 local_memory_id TEXT PRIMARY KEY NOT NULL,
                 project TEXT NOT NULL
                     CHECK (project <> '__global__'),
                 gateway_memory_id TEXT NOT NULL,
                 last_seen_server_revision INTEGER NOT NULL,
                 last_pushed_content_hash TEXT,
                 last_pulled_content_hash TEXT,
                 sync_state TEXT NOT NULL,
                 tombstone_deleted INTEGER NOT NULL DEFAULT 0
                     CHECK (tombstone_deleted IN (0, 1)),
                 tombstone_at TEXT,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 FOREIGN KEY(local_memory_id) REFERENCES memories(id) ON DELETE CASCADE,
                 UNIQUE(project, gateway_memory_id)
             );
             CREATE TABLE project_gateway_sync_state (
                 project TEXT PRIMARY KEY NOT NULL
                     CHECK (project <> '__global__'),
                 last_pull_server_revision INTEGER,
                 last_pull_cursor TEXT,
                 updated_at TEXT NOT NULL
             );
             INSERT INTO schema_version (version) VALUES (1);
             INSERT INTO schema_version (version) VALUES (2);
             INSERT INTO schema_version (version) VALUES (3);
             INSERT INTO schema_version (version) VALUES (4);
             INSERT INTO schema_version (version) VALUES (5);
             INSERT INTO schema_version (version) VALUES (6);
             INSERT INTO memories (id, content, project, created_at, updated_at)
                 VALUES ('global-row', 'global body', '__global__', '2026-01-01', '2026-01-01');",
        )
        .expect("seed v6 db");

        run_migrations(&conn).expect("migrate to latest");

        let max_v: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .expect("query schema_version");
        assert_eq!(max_v, 12);

        conn.execute(
            "INSERT INTO memory_gateway_sync (
                 local_memory_id, project, gateway_memory_id,
                 last_seen_server_revision, sync_state,
                 tombstone_deleted, created_at, updated_at
             )
             VALUES (
                 'global-row', '__global__', 'gw-global',
                 1, 'pulled', 0, '2026-01-01', '2026-01-01'
             )",
            [],
        )
        .expect("insert global gateway sync row");

        conn.execute(
            "INSERT INTO project_gateway_sync_state (
                 project, last_pull_server_revision, last_pull_cursor, updated_at
             )
             VALUES ('__global__', 1, 'cursor', '2026-01-01')",
            [],
        )
        .expect("insert global project cursor");
    }

    /// Content + content_raw are concatenated in the FTS index so terms that
    /// only appear in the raw field remain lexically searchable after dream
    /// condenses a memory.
    #[test]
    fn fts_triggers_index_content_raw_after_v3() {
        let conn = Connection::open_in_memory().expect("open db");
        run_migrations(&conn).expect("migrate");

        conn.execute(
            "INSERT INTO memories (id, content, content_raw, created_at, updated_at)
             VALUES ('x', 'short summary', 'full verbatim needle in raw', '2026-01-01', '2026-01-01')",
            [],
        )
        .expect("insert row with content_raw");

        // 'needle' is only in content_raw — FTS must still find it.
        let needle_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'needle'",
                [],
                |row| row.get(0),
            )
            .expect("fts query");
        assert_eq!(needle_hits, 1);

        // 'summary' is only in content — FTS finds it too.
        let summary_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'summary'",
                [],
                |row| row.get(0),
            )
            .expect("fts query");
        assert_eq!(summary_hits, 1);
    }

    #[test]
    fn v8_database_upgrades_to_okf_native_concepts_without_touching_other_state() {
        let conn = Connection::open_in_memory().expect("open db");
        run_migrations(&conn).expect("create latest schema");

        // Recreate the exact pre-v9 condition in a separate connection by
        // removing only v9-derived state and its version marker.
        conn.execute_batch(
            "DROP TRIGGER memory_segments_fts_ad;
             DROP TABLE memory_segments_fts;
             DROP TABLE memory_relationships;
             DROP TABLE memory_verifications;
             DROP TABLE memory_sources;
             DROP TABLE memory_revisions;
             DROP TABLE memory_concepts;
             DROP TABLE memory_concept_tombstones;
             DELETE FROM schema_version WHERE version IN (9, 10, 11, 12);
             INSERT INTO memories (
                 id, content, tags, project, agent, source_file,
                 created_at, updated_at, access_count, embedding, memory_type,
                 content_raw, superseded_by, condenser_version, embedding_model
             ) VALUES (
                 'legacy-id', 'body bytes', '[\"one\",\"two\"]', 'agent-memory',
                 'legacy-agent', 'memory/src/lib.rs', '2026-01-01', '2026-02-01',
                 7, X'0000803F', 'project', 'original body', NULL,
                 'dream/v1', 'all-MiniLM-L6-v2'
             );
             INSERT INTO project_working_context (project, content, version, updated_at)
                 VALUES ('agent-memory', 'do not project me', 2, '2026-02-01');
             INSERT INTO memory_gateway_sync (
                 local_memory_id, project, gateway_memory_id,
                 last_seen_server_revision, sync_state, tombstone_deleted,
                 created_at, updated_at
             ) VALUES (
                 'legacy-id', 'agent-memory', 'gateway-id', 4, 'pulled', 0,
                 '2026-02-01', '2026-02-01'
             );",
        )
        .expect("seed schema 8 state");

        run_migrations(&conn).expect("upgrade v8 to v9");
        run_migrations(&conn).expect("idempotent reopen");

        let concept: (String, String, i64, String, String, String) = conn
            .query_row(
                "SELECT concept_type, status, current_revision, virtual_path,
                        created_at, updated_at
                 FROM memory_concepts WHERE memory_id = 'legacy-id'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("query concept");
        assert_eq!(
            concept,
            (
                "Agent Memory/project".to_string(),
                "stable".to_string(),
                1,
                "/memories/legacy-id.md".to_string(),
                "2026-01-01".to_string(),
                "2026-02-01".to_string(),
            )
        );

        let revision: (String, String, String) = conn
            .query_row(
                "SELECT operation, snapshot_json, content_hash
                 FROM memory_revisions WHERE memory_id = 'legacy-id'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query revision");
        assert_eq!(revision.0, "migrate");
        assert_eq!(revision.2.len(), 64);
        let snapshot: serde_json::Value =
            serde_json::from_str(&revision.1).expect("valid snapshot JSON");
        assert_eq!(snapshot["body"], "body bytes");
        assert_eq!(snapshot["content_raw"], "original body");
        assert_eq!(snapshot["tags"], serde_json::json!(["one", "two"]));
        assert_eq!(snapshot["okf"]["type"], "Agent Memory/project");

        let unchanged: (String, i64, Vec<u8>, String) = conn
            .query_row(
                "SELECT content, access_count, embedding, condenser_version
                 FROM memories WHERE id = 'legacy-id'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("query original memory");
        assert_eq!(unchanged.0, "body bytes");
        assert_eq!(unchanged.1, 7);
        assert_eq!(unchanged.2, vec![0, 0, 128, 63]);
        assert_eq!(unchanged.3, "dream/v1");

        let gateway_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_gateway_sync
                 WHERE local_memory_id = 'legacy-id' AND gateway_memory_id = 'gateway-id'",
                [],
                |row| row.get(0),
            )
            .expect("query gateway mapping");
        assert_eq!(gateway_count, 1);

        let working_concepts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_concepts
                 WHERE memory_id IN (SELECT project FROM project_working_context)",
                [],
                |row| row.get(0),
            )
            .expect("query working context isolation");
        assert_eq!(working_concepts, 0);
    }

    #[test]
    fn v9_constraints_reject_or_cascade_inconsistent_concept_state() {
        let conn = Connection::open_in_memory().expect("open db");
        conn.pragma_update(None, "foreign_keys", "ON")
            .expect("enable foreign keys");
        run_migrations(&conn).expect("migrate");
        conn.execute(
            "INSERT INTO memories (id, content, created_at, updated_at)
             VALUES ('parent', 'body', '2026-01-01', '2026-01-01')",
            [],
        )
        .expect("insert memory");

        assert!(conn
            .execute(
                "INSERT INTO memory_concepts (
                     memory_id, concept_type, status, current_revision,
                     virtual_path, created_at, updated_at
                 ) VALUES ('missing', 'Reference', 'stable', 1,
                           '/memories/missing.md', 'now', 'now')",
                [],
            )
            .is_err());
        assert!(conn
            .execute(
                "INSERT INTO memory_concepts (
                     memory_id, concept_type, status, current_revision,
                     virtual_path, created_at, updated_at
                 ) VALUES ('parent', '', 'stable', 1,
                           '/memories/parent.md', 'now', 'now')",
                [],
            )
            .is_err());
    }

    #[test]
    fn v9_database_rebuilds_okf_search_segments_on_v10_upgrade() {
        let conn = Connection::open_in_memory().expect("open db");
        run_migrations(&conn).expect("create latest schema");
        let memory = crate::db::models::Memory::new(
            "legacy searchable body".to_string(),
            Some(vec!["legacy-tag".to_string()]),
            Some("project".to_string()),
            None,
            None,
            Some("reference".to_string()),
        );
        let id = memory.id.clone();
        crate::db::queries::insert_memory(&conn, &memory).expect("insert memory");
        conn.execute_batch(
            "DROP TRIGGER memory_segments_fts_ad;
             DROP TABLE memory_segments_fts;
             DELETE FROM schema_version WHERE version IN (10, 11, 12);",
        )
        .expect("restore v9 state");

        run_migrations(&conn).expect("upgrade to v10");
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_segments_fts
                 WHERE memory_segments_fts MATCH 'legacy' AND memory_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .expect("search rebuilt projection");
        assert_eq!(hits, 1);
    }

    #[test]
    fn migration_failure_rolls_back_and_clean_retry_succeeds() {
        let conn = Connection::open_in_memory().expect("open db");
        run_migrations(&conn).expect("create latest schema");
        let memory = crate::db::models::Memory::new(
            "force projection rebuild".to_string(),
            None,
            Some("project".to_string()),
            None,
            None,
            Some("project".to_string()),
        );
        crate::db::queries::insert_memory(&conn, &memory).unwrap();
        conn.execute_batch(
            "DROP TRIGGER memory_segments_fts_ad;
             DROP TABLE memory_segments_fts;
             DELETE FROM schema_version WHERE version IN (10, 11, 12);
             CREATE TABLE memory_segments_fts (wrong_column TEXT);",
        )
        .expect("construct incompatible v9 fixture");

        assert!(run_migrations(&conn).is_err());
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 9, "failed migration must not advance the schema");
        let columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('memory_segments_fts')
                 WHERE name = 'wrong_column'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 1, "failed migration must roll back its DDL");

        conn.execute("DROP TABLE memory_segments_fts", []).unwrap();
        run_migrations(&conn).expect("clean retry");
        run_migrations(&conn).expect("idempotent reopen");
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 12);
    }

    #[test]
    fn v11_repairs_relationships_owned_by_dream_revisions() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let memory = crate::db::models::Memory::new(
            "replacement".to_string(),
            None,
            Some("project".to_string()),
            None,
            None,
            Some("project".to_string()),
        );
        let id = memory.id.clone();
        crate::db::queries::insert_memory(&conn, &memory).unwrap();
        conn.execute(
            "UPDATE memory_revisions SET actor = 'memory-dream'
             WHERE memory_id = ?1 AND revision = 1",
            params![id],
        )
        .unwrap();
        crate::concepts::insert_relationship(
            &conn,
            &id,
            None,
            "memory://project/project/original",
            "supersedes",
            1,
            None,
        )
        .unwrap();
        // Migrations gate on MAX(version), so every row at or above the one
        // under test has to go for its guard to re-open.
        conn.execute("DELETE FROM schema_version WHERE version >= 11", [])
            .unwrap();

        run_migrations(&conn).unwrap();

        let producer: String = conn
            .query_row(
                "SELECT producer FROM memory_relationships WHERE src_memory_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(producer, "memory-dream");
    }

    #[test]
    fn v12_namespaces_dream_operations_without_touching_other_writers() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let mut expected = Vec::new();
        for (actor, operation, want) in [
            (Some("memory-dream"), "condense", "dream_condense"),
            (Some("memory-dream"), "extract", "dream_extract"),
            (Some("memory-dream"), "supersede", "dream_supersede"),
            (Some("memory-dream"), "dream_merge", "dream_merge"),
            // Another writer's bare verb keeps its own history.
            (Some("someone-else"), "condense", "condense"),
            (None, "store", "store"),
        ] {
            let memory = crate::db::models::Memory::new(
                format!("body {operation} {actor:?}"),
                None,
                Some("project".to_string()),
                None,
                None,
                Some("project".to_string()),
            );
            let id = memory.id.clone();
            crate::db::queries::insert_memory(&conn, &memory).unwrap();
            conn.execute(
                "UPDATE memory_revisions SET actor = ?1, operation = ?2
                 WHERE memory_id = ?3 AND revision = 1",
                params![actor, operation, id],
            )
            .unwrap();
            expected.push((id, want));
        }
        conn.execute("DELETE FROM schema_version WHERE version >= 12", [])
            .unwrap();

        run_migrations(&conn).unwrap();

        for (id, want) in expected {
            let operation: String = conn
                .query_row(
                    "SELECT operation FROM memory_revisions WHERE memory_id = ?1 AND revision = 1",
                    params![id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(operation, want);
        }
    }

    #[test]
    fn wal_reader_remains_available_while_writer_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrency.db");
        let writer = open_database(&path).unwrap();
        writer
            .execute(
                "INSERT INTO memories (id, content, created_at, updated_at)
                 VALUES ('visible', 'before', '2026-01-01', '2026-01-01')",
                [],
            )
            .unwrap();
        let reader = open_database(&path).unwrap();

        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer
            .execute(
                "UPDATE memories SET content = 'uncommitted' WHERE id = 'visible'",
                [],
            )
            .unwrap();
        let observed: String = reader
            .query_row(
                "SELECT content FROM memories WHERE id = 'visible'",
                [],
                |row| row.get(0),
            )
            .expect("WAL reader should not wait for writer");
        assert_eq!(observed, "before");
        writer.execute_batch("ROLLBACK").unwrap();
    }
}
