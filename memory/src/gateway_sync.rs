use std::env;

use rusqlite::{Connection, OptionalExtension};

use crate::config::GatewayConfig;
use crate::db::models::{Memory, MemoryGatewaySync, MemoryGatewaySyncUpsert};
use crate::db::queries;
use crate::error::MemoryError;
use crate::sync::{
    gateway_okf_envelope, memory_content_hash, GatewayMemory, GatewayMemoryProvenance,
    GatewayMemoryTombstone, GatewaySyncClientError, MemoryGatewayClient, PullMemoriesRequest,
    PushMemoriesRequest, PushMemoriesResponse, PushMemoryAction, PushMemoryResult,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayMutationOutcome {
    Skipped {
        reason: &'static str,
    },
    Synced {
        action: PushMemoryAction,
        gateway_memory_id: Option<String>,
        server_revision: Option<i64>,
    },
}

impl GatewayMutationOutcome {
    pub fn was_synced(&self) -> bool {
        matches!(self, Self::Synced { .. })
    }
}

/// The legacy tables have one mapping per local memory. Bind the entire sync
/// state to its endpoint rather than silently using it on a different server.
/// A different endpoint requires a separate database; no mappings are discarded.
pub fn ensure_gateway_identity(
    conn: &Connection,
    client: &MemoryGatewayClient,
    bind: bool,
) -> Result<(), MemoryError> {
    if bind {
        conn.execute(
            "INSERT OR IGNORE INTO gateway_sync_identity(singleton, endpoint) VALUES(1, ?1)",
            [client.identity()],
        )?;
    }
    let endpoint: Option<String> = conn
        .query_row(
            "SELECT endpoint FROM gateway_sync_identity WHERE singleton = 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if endpoint.as_deref().is_some_and(|e| e != client.identity()) {
        return Err(MemoryError::Config(
            "gateway identity changed; use a separate memory database for this endpoint to preserve existing mappings and cursors".into(),
        ));
    }
    Ok(())
}

/// Global scope has a restricted legacy wire type. The local concept and its
/// OKF envelope retain their original type (including reference).
pub fn wire_memory_type<'a>(project: &str, local_type: Option<&'a str>) -> &'a str {
    if local_type == Some("working_context") {
        "working_context"
    } else if project == queries::GLOBAL_PROJECT_IDENT {
        "global"
    } else {
        local_type.unwrap_or("user")
    }
}

pub fn push_reconciled(
    conn: &Connection,
    client: &MemoryGatewayClient,
    request: &PushMemoriesRequest,
) -> Result<PushMemoriesResponse, MemoryError> {
    ensure_gateway_identity(conn, client, true)?;
    let mut response = client.push_memories(request).map_err(map_gateway_error)?;
    if response.project != request.project {
        return Err(MemoryError::Config(
            "gateway push response project mismatch".into(),
        ));
    }
    // Recovery is bounded to one verified retry per linked record. A conflict
    // does not itself authorize overwriting data or resurrecting a tombstone.
    for result in &mut response.results {
        if !matches!(
            result.action,
            PushMemoryAction::Conflict | PushMemoryAction::Rejected
        ) {
            continue;
        }
        let Some(memory) = request.memories.iter().find(|m| {
            m.local_memory_id
                .as_ref()
                .is_some_and(|id| result.local_memory_id.as_ref() == Some(id))
                || m.client_id
                    .as_ref()
                    .is_some_and(|id| result.client_id.as_ref() == Some(id))
                || m.gateway_memory_id
                    .as_ref()
                    .is_some_and(|id| result.gateway_memory_id.as_ref() == Some(id))
        }) else {
            continue;
        };
        if memory.gateway_memory_id.is_none() {
            continue;
        }
        let deleting = memory.tombstone.as_ref().is_some_and(|t| t.deleted);
        if deleting && tombstone_target_is_already_absent(result) {
            continue;
        }
        // Validation errors unrelated to stale IDs/revisions are not retried.
        if result.action != PushMemoryAction::Conflict
            && !tombstone_target_is_already_absent(result)
        {
            continue;
        }
        let snapshot = remote_snapshot(client, &request.project)?;
        let matches: Vec<_> = snapshot
            .iter()
            .filter(|r| {
                r.gateway_memory_id == memory.gateway_memory_id
                    || memory
                        .client_id
                        .as_ref()
                        .is_some_and(|id| r.client_id.as_ref() == Some(id))
            })
            .collect();
        let mut retry = memory.clone();
        match matches.as_slice() {
            [] if tombstone_target_is_already_absent(result) && !deleting => {
                retry.gateway_memory_id = None;
                retry.base_server_revision = None;
            }
            [remote] => {
                let Some(revision) = remote.server_revision else {
                    continue;
                };
                if remote.tombstone.as_ref().is_some_and(|t| t.deleted) {
                    if deleting {
                        result.action = PushMemoryAction::Tombstoned;
                        result.gateway_memory_id = remote.gateway_memory_id.clone();
                        result.server_revision = Some(revision);
                        result.content_hash = Some(remote.content_hash.clone());
                        result.conflict = None;
                        result.error = None;
                        result.errors.clear();
                    }
                    continue;
                }
                let sync = match memory.local_memory_id.as_deref() {
                    Some(id) => queries::get_memory_gateway_sync(conn, id)?
                        .or(queries::get_memory_gateway_delete_pending(conn, id)?),
                    None => None,
                };
                let baseline_matches = sync.as_ref().is_some_and(|s| {
                    s.last_pushed_content_hash.as_deref() == Some(&remote.content_hash)
                        || s.last_pulled_content_hash.as_deref() == Some(&remote.content_hash)
                });
                let retained_ancestor = match sync.as_ref() {
                    Some(sync) if revision < sync.last_seen_server_revision => {
                        remote_matches_retained_history(conn, &sync.local_memory_id, remote)?
                    }
                    _ => false,
                };
                // With OKF enabled, legacy equality alone cannot prove that
                // independently edited metadata is safe to replace.
                let okf_matches = memory.okf.is_none()
                    || (memory.concept_hash.is_some()
                        && memory.concept_hash == remote.concept_hash);
                if !okf_matches
                    || (!baseline_matches
                        && !retained_ancestor
                        && memory.content_hash != remote.content_hash)
                {
                    continue;
                }
                retry.gateway_memory_id = remote.gateway_memory_id.clone();
                retry.base_server_revision = Some(revision);
            }
            _ => continue, // ambiguous identity: leave the conflict intact
        }
        let retried = client
            .push_memories(&PushMemoriesRequest {
                project: request.project.clone(),
                memories: vec![retry.clone()],
            })
            .map_err(map_gateway_error)?;
        if retried.project != request.project || retried.results.len() != 1 {
            return Err(MemoryError::Config(
                "invalid gateway reconciliation response".into(),
            ));
        }
        let replacement = &retried.results[0];
        if replacement.local_memory_id != retry.local_memory_id
            && replacement.client_id != retry.client_id
        {
            return Err(MemoryError::Config(
                "gateway reconciliation result identity mismatch".into(),
            ));
        }
        *result = replacement.clone();
    }
    Ok(response)
}

/// A lower remote revision may reflect an older retained body. Only exact
/// retained content/type/tags in the same scope authorize advancing it again;
/// an unseen remote edit or a newer remote revision is never overwritten here.
fn remote_matches_retained_history(
    conn: &Connection,
    id: &str,
    remote: &GatewayMemory,
) -> Result<bool, MemoryError> {
    if remote.content_hash
        != memory_content_hash(&remote.content, &remote.memory_type, &remote.tags)
    {
        return Ok(false);
    }
    let mut statement =
        conn.prepare("SELECT snapshot_json FROM memory_revisions WHERE memory_id = ?1")?;
    let rows = statement.query_map([id], |r| r.get::<_, String>(0))?;
    let mut remote_tags = remote.tags.clone();
    remote_tags.sort();
    remote_tags.dedup();
    for row in rows {
        let snapshot: serde_json::Value =
            serde_json::from_str(&row?).map_err(|e| MemoryError::Config(e.to_string()))?;
        let content_matches = snapshot["body"].as_str() == Some(&remote.content)
            || snapshot["content_raw"].as_str() == Some(&remote.content);
        let tags: Vec<String> = serde_json::from_value(snapshot["tags"].clone())
            .map_err(|e| MemoryError::Config(e.to_string()))?;
        if content_matches
            && snapshot["project"].as_str() == Some(&remote.project)
            && wire_memory_type(&remote.project, snapshot["memory_type"].as_str())
                == remote.memory_type
            && tags == remote_tags
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remote_snapshot(
    client: &MemoryGatewayClient,
    project: &str,
) -> Result<Vec<GatewayMemory>, MemoryError> {
    let mut cursor = None;
    let mut since = None;
    let mut seen = std::collections::HashSet::new();
    let mut memories = Vec::new();
    loop {
        let page = client
            .pull_memories(&PullMemoriesRequest {
                project: project.into(),
                since_server_revision: since,
                cursor: cursor.clone(),
                known_memories: Vec::new(),
                limit: Some(100),
            })
            .map_err(map_gateway_error)?;
        page.validate_project_scope()
            .map_err(|e| MemoryError::Config(e.to_string()))?;
        if page.project != project {
            return Err(MemoryError::Config(
                "gateway snapshot project mismatch".into(),
            ));
        }
        memories.extend(page.memories);
        if !page.has_more {
            return Ok(memories);
        }
        let position = if let Some(next) = page.next_cursor {
            cursor = Some(next.clone());
            format!("cursor:{next}")
        } else if let Some(next) = page.next_since_revision.filter(|n| *n > since.unwrap_or(0)) {
            since = Some(next);
            format!("revision:{next}")
        } else {
            return Err(MemoryError::Config(
                "gateway snapshot missing advancing continuation".into(),
            ));
        };
        if !seen.insert(position) {
            return Err(MemoryError::Config(
                "gateway snapshot repeated continuation".into(),
            ));
        }
    }
}

pub fn push_memory_update_if_configured(
    conn: &Connection,
    gateway_config: &GatewayConfig,
    memory: &Memory,
) -> Result<GatewayMutationOutcome, MemoryError> {
    let Some(project) = gateway_project_for_memory(gateway_config, memory) else {
        return Ok(GatewayMutationOutcome::Skipped {
            reason: "gateway auto-sync disabled or memory is unscoped",
        });
    };

    let sync = queries::get_memory_gateway_sync(conn, &memory.id)?;
    if sync.as_ref().is_some_and(|s| s.tombstone_deleted) {
        return Err(MemoryError::Config(
            "gateway update refused: linked memory is tombstoned".into(),
        ));
    }
    let gateway_memory = gateway_memory_from_local(
        conn,
        memory,
        project,
        sync.as_ref(),
        gateway_config.okf_sync_enabled(),
    )?;
    let content_hash = gateway_memory.content_hash.clone();
    let result = push_single(conn, gateway_config, gateway_memory)?;

    match result.action {
        PushMemoryAction::Created | PushMemoryAction::Updated | PushMemoryAction::Linked => {
            record_successful_push(
                conn,
                project,
                &memory.id,
                &result,
                Some(content_hash),
                false,
            )?;
            Ok(outcome_from_result(result))
        }
        PushMemoryAction::Conflict | PushMemoryAction::Rejected => {
            Err(push_result_error("gateway update", &result))
        }
        PushMemoryAction::Deleted | PushMemoryAction::Tombstoned => Err(MemoryError::Config(
            "gateway update unexpectedly returned a delete/tombstone action".to_string(),
        )),
    }
}

/// Push a gateway tombstone before a local delete or supersede hides the row.
///
/// If the row has never been linked to the gateway there is no remote record
/// to delete, so the caller can proceed with the local mutation. For linked
/// records, errors are returned and callers should keep the local row intact so
/// a later retry still has the gateway ID and base revision needed to tombstone
/// the remote record.
pub fn tombstone_memory_before_local_removal(
    conn: &Connection,
    gateway_config: &GatewayConfig,
    memory: &Memory,
    reason: &str,
) -> Result<GatewayMutationOutcome, MemoryError> {
    let Some(project) = gateway_project_for_memory(gateway_config, memory) else {
        return Ok(GatewayMutationOutcome::Skipped {
            reason: "gateway auto-sync disabled or memory is unscoped",
        });
    };
    let Some(sync) = queries::get_memory_gateway_sync(conn, &memory.id)?.or(
        queries::get_memory_gateway_delete_pending(conn, &memory.id)?,
    ) else {
        return Ok(GatewayMutationOutcome::Skipped {
            reason: "memory is not linked to gateway",
        });
    };
    let client = MemoryGatewayClient::from_config(gateway_config).map_err(map_gateway_error)?;
    ensure_gateway_identity(conn, &client, true)?;
    if sync.tombstone_deleted && sync.sync_state != "delete_pending" {
        return Ok(GatewayMutationOutcome::Skipped {
            reason: "gateway tombstone already recorded",
        });
    }

    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
        [&memory.id],
        |r| r.get(0),
    )?;
    let gateway_memory = if exists {
        gateway_tombstone_from_local(
            conn,
            memory,
            project,
            &sync,
            reason,
            gateway_config.okf_sync_enabled(),
        )?
    } else {
        gateway_tombstone_from_sync(&sync, reason)
    };
    let content_hash = gateway_memory.content_hash.clone();
    let result = push_single(conn, gateway_config, gateway_memory)?;

    match result.action {
        PushMemoryAction::Updated | PushMemoryAction::Deleted | PushMemoryAction::Tombstoned => {
            record_successful_push(conn, project, &memory.id, &result, Some(content_hash), true)?;
            Ok(outcome_from_result(result))
        }
        PushMemoryAction::Created | PushMemoryAction::Linked => Err(MemoryError::Config(format!(
            "gateway tombstone returned unexpected action {} for memory {}",
            push_action_label(&result.action),
            memory.id
        ))),
        PushMemoryAction::Conflict | PushMemoryAction::Rejected => {
            if tombstone_target_is_already_absent(&result) {
                record_already_absent_tombstone(conn, &sync)?;
                Ok(GatewayMutationOutcome::Skipped {
                    reason: "gateway tombstone target already absent",
                })
            } else {
                Err(push_result_error("gateway tombstone", &result))
            }
        }
    }
}

fn gateway_project_for_memory<'a>(
    gateway_config: &GatewayConfig,
    memory: &'a Memory,
) -> Option<&'a str> {
    if !gateway_config.auto_sync_enabled() {
        return None;
    }
    if memory.memory_type.as_deref() == Some("working_context") {
        return None;
    }
    memory.project.as_deref().filter(|p| !p.trim().is_empty())
}

fn gateway_memory_from_local(
    conn: &Connection,
    memory: &Memory,
    project: &str,
    sync: Option<&MemoryGatewaySync>,
    okf_enabled: bool,
) -> Result<GatewayMemory, MemoryError> {
    if memory.project.as_deref() != Some(project) {
        return Err(MemoryError::Config(format!(
            "memory {} is not scoped to project {project}",
            memory.id
        )));
    }
    let memory_type = wire_memory_type(project, memory.memory_type.as_deref()).to_string();
    let tags = memory.tags.clone().unwrap_or_default();
    let content_hash = memory_content_hash(&memory.content, &memory_type, &tags);
    let okf = okf_enabled
        .then(|| gateway_okf_envelope(conn, memory))
        .transpose()?;
    let concept_hash = okf.as_ref().map(|envelope| envelope.semantic_hash.clone());
    Ok(GatewayMemory {
        project: project.to_string(),
        content: memory.content.clone(),
        memory_type,
        tags,
        content_hash,
        concept_hash,
        okf,
        local_memory_id: Some(memory.id.clone()),
        client_id: Some(memory.id.clone()),
        gateway_memory_id: sync.map(|record| record.gateway_memory_id.clone()),
        base_server_revision: sync.map(|record| record.last_seen_server_revision),
        server_revision: None,
        created_at: Some(memory.created_at.clone()),
        updated_at: Some(memory.updated_at.clone()),
        provenance: Some(gateway_provenance_from_local(memory)),
        tombstone: None,
    })
}

fn gateway_tombstone_from_local(
    conn: &Connection,
    memory: &Memory,
    project: &str,
    sync: &MemoryGatewaySync,
    reason: &str,
    okf_enabled: bool,
) -> Result<GatewayMemory, MemoryError> {
    let mut gateway_memory =
        gateway_memory_from_local(conn, memory, project, Some(sync), okf_enabled)?;
    gateway_memory.tombstone = Some(GatewayMemoryTombstone {
        deleted: true,
        deleted_at: Some(chrono::Utc::now().to_rfc3339()),
        reason: Some(reason.to_string()),
    });
    Ok(gateway_memory)
}

pub fn gateway_tombstone_from_sync(record: &MemoryGatewaySync, reason: &str) -> GatewayMemory {
    let content_hash = sync_content_hash(record)
        .map(str::to_string)
        .unwrap_or_else(|| memory_content_hash("", "project", &[]));
    let tombstone_at = record
        .tombstone_at
        .clone()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    GatewayMemory {
        project: record.project.clone(),
        content: String::new(),
        memory_type: wire_memory_type(&record.project, Some("project")).to_string(),
        tags: Vec::new(),
        content_hash,
        concept_hash: None,
        okf: None,
        local_memory_id: Some(record.local_memory_id.clone()),
        client_id: Some(record.local_memory_id.clone()),
        gateway_memory_id: Some(record.gateway_memory_id.clone()),
        base_server_revision: Some(record.last_seen_server_revision),
        server_revision: None,
        created_at: None,
        updated_at: record.tombstone_at.clone(),
        provenance: Some(GatewayMemoryProvenance {
            source_agent_id: None,
            source_machine_id: local_host_alias(),
            source_os: Some(env::consts::OS.to_string()),
            source_arch: Some(env::consts::ARCH.to_string()),
            source_system: Some("agent-memory".to_string()),
            pushed_at: Some(chrono::Utc::now().to_rfc3339()),
        }),
        tombstone: Some(GatewayMemoryTombstone {
            deleted: true,
            deleted_at: Some(tombstone_at),
            reason: Some(reason.to_string()),
        }),
    }
}

fn push_single(
    conn: &Connection,
    gateway_config: &GatewayConfig,
    gateway_memory: GatewayMemory,
) -> Result<PushMemoryResult, MemoryError> {
    let project = gateway_memory.project.clone();
    let local_id = gateway_memory.local_memory_id.clone();
    let client_id = gateway_memory.client_id.clone();
    let gateway_id = gateway_memory.gateway_memory_id.clone();
    let request = PushMemoriesRequest {
        project,
        memories: vec![gateway_memory],
    };
    let client = MemoryGatewayClient::from_config(gateway_config).map_err(map_gateway_error)?;
    let response = push_reconciled(conn, &client, &request)?;

    let mut results = response.results;
    if let Some(index) = results.iter().position(|result| {
        local_id
            .as_deref()
            .is_some_and(|id| result.local_memory_id.as_deref() == Some(id))
            || client_id
                .as_deref()
                .is_some_and(|id| result.client_id.as_deref() == Some(id))
            || gateway_id
                .as_deref()
                .is_some_and(|id| result.gateway_memory_id.as_deref() == Some(id))
    }) {
        return Ok(results.remove(index));
    }
    Err(MemoryError::Config(
        "gateway push response did not include a result for the memory".to_string(),
    ))
}

fn record_successful_push(
    conn: &Connection,
    project: &str,
    local_memory_id: &str,
    result: &PushMemoryResult,
    fallback_content_hash: Option<String>,
    tombstone_deleted: bool,
) -> Result<(), MemoryError> {
    let gateway_memory_id = result.gateway_memory_id.as_deref().ok_or_else(|| {
        MemoryError::Config("gateway push response missing gateway_memory_id".to_string())
    })?;
    let server_revision = result.server_revision.ok_or_else(|| {
        MemoryError::Config("gateway push response missing server_revision".to_string())
    })?;
    if tombstone_deleted
        && !conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
            [local_memory_id],
            |r| r.get::<_, bool>(0),
        )?
    {
        queries::clear_memory_gateway_delete_pending(conn, local_memory_id)?;
        return Ok(());
    }
    let content_hash = result.content_hash.clone().or(fallback_content_hash);
    let tombstone_at = if tombstone_deleted {
        Some(chrono::Utc::now().to_rfc3339())
    } else {
        None
    };
    let sync_state = if tombstone_deleted {
        "tombstoned"
    } else {
        push_action_label(&result.action)
    };

    queries::upsert_memory_gateway_sync(
        conn,
        &MemoryGatewaySyncUpsert {
            local_memory_id: local_memory_id.to_string(),
            project: project.to_string(),
            gateway_memory_id: gateway_memory_id.to_string(),
            last_seen_server_revision: server_revision,
            last_pushed_content_hash: content_hash,
            last_pulled_content_hash: None,
            sync_state: sync_state.to_string(),
            tombstone_deleted,
            tombstone_at,
        },
    )?;
    Ok(())
}

fn outcome_from_result(result: PushMemoryResult) -> GatewayMutationOutcome {
    GatewayMutationOutcome::Synced {
        action: result.action,
        gateway_memory_id: result.gateway_memory_id,
        server_revision: result.server_revision,
    }
}

fn push_result_error(operation: &str, result: &PushMemoryResult) -> MemoryError {
    let mut message = format!("{operation} {}", push_action_label(&result.action));
    if let Some(conflict) = result.conflict.as_ref() {
        message.push_str(": ");
        message.push_str(&conflict.reason);
    } else if let Some(error) = result.error.as_deref() {
        message.push_str(": ");
        message.push_str(error);
    } else if let Some(error) = result.errors.first() {
        message.push_str(": ");
        message.push_str(&error.message);
    }
    MemoryError::Config(message)
}

fn tombstone_target_is_already_absent(result: &PushMemoryResult) -> bool {
    if !matches!(
        result.action,
        PushMemoryAction::Conflict | PushMemoryAction::Rejected
    ) {
        return false;
    }
    let missing = |message: &str| {
        let message = message.to_ascii_lowercase();
        message.contains("gateway_memory_id was not found")
            || message.contains("gateway memory id was not found")
            || message.contains("gateway memory not found")
    };
    result
        .conflict
        .as_ref()
        .is_some_and(|conflict| missing(&conflict.reason))
        || result.error.as_deref().is_some_and(missing)
        || result.errors.iter().any(|error| missing(&error.message))
}

fn record_already_absent_tombstone(
    conn: &Connection,
    sync: &MemoryGatewaySync,
) -> Result<(), MemoryError> {
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
        [&sync.local_memory_id],
        |r| r.get::<_, bool>(0),
    )? {
        queries::clear_memory_gateway_delete_pending(conn, &sync.local_memory_id)?;
        return Ok(());
    }
    queries::upsert_memory_gateway_sync(
        conn,
        &MemoryGatewaySyncUpsert {
            local_memory_id: sync.local_memory_id.clone(),
            project: sync.project.clone(),
            gateway_memory_id: sync.gateway_memory_id.clone(),
            last_seen_server_revision: sync.last_seen_server_revision,
            last_pushed_content_hash: sync.last_pushed_content_hash.clone(),
            last_pulled_content_hash: sync.last_pulled_content_hash.clone(),
            sync_state: "already_absent".to_string(),
            tombstone_deleted: true,
            tombstone_at: Some(chrono::Utc::now().to_rfc3339()),
        },
    )?;
    Ok(())
}

fn push_action_label(action: &PushMemoryAction) -> &'static str {
    match action {
        PushMemoryAction::Created => "created",
        PushMemoryAction::Updated => "updated",
        PushMemoryAction::Linked => "linked",
        PushMemoryAction::Deleted => "deleted",
        PushMemoryAction::Tombstoned => "tombstoned",
        PushMemoryAction::Conflict => "conflict",
        PushMemoryAction::Rejected => "rejected",
    }
}

fn sync_content_hash(record: &MemoryGatewaySync) -> Option<&str> {
    record
        .last_pushed_content_hash
        .as_deref()
        .or(record.last_pulled_content_hash.as_deref())
}

fn gateway_provenance_from_local(memory: &Memory) -> GatewayMemoryProvenance {
    GatewayMemoryProvenance {
        source_agent_id: memory.agent.clone(),
        source_machine_id: local_host_alias(),
        source_os: Some(env::consts::OS.to_string()),
        source_arch: Some(env::consts::ARCH.to_string()),
        source_system: Some("agent-memory".to_string()),
        pushed_at: Some(chrono::Utc::now().to_rfc3339()),
    }
}

fn local_host_alias() -> Option<String> {
    ["AGENT_MEMORY_HOST", "HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|key| {
            env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn map_gateway_error(err: GatewaySyncClientError) -> MemoryError {
    match err {
        GatewaySyncClientError::Config(msg) => MemoryError::Config(msg),
        GatewaySyncClientError::Authentication(msg) => MemoryError::Config(msg),
        GatewaySyncClientError::ProjectAuthorization(msg) => MemoryError::Config(msg),
        GatewaySyncClientError::Validation(msg) => MemoryError::Config(msg),
        GatewaySyncClientError::Transient(msg) => MemoryError::Config(msg),
        GatewaySyncClientError::Transport(msg) => MemoryError::Config(msg),
        GatewaySyncClientError::MalformedResponse(msg) => MemoryError::Config(msg),
        GatewaySyncClientError::Scope(err) => MemoryError::Config(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, queries};
    use serde_json::{json, Value};
    use std::io::{Read, Write};

    fn open_mem_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory");
        db::run_migrations(&conn).expect("migrate");
        conn
    }

    fn gateway_config(base_url: String) -> GatewayConfig {
        GatewayConfig {
            base_url: Some(base_url),
            api_key: Some("test-key".to_string()),
            auto_sync: Some(true),
            okf_sync: None,
        }
    }

    fn insert_memory(conn: &Connection, id: &str, content: &str) -> Memory {
        let mut memory = Memory::new(
            content.to_string(),
            Some(vec!["gateway".to_string()]),
            Some("agent-memory".to_string()),
            None,
            None,
            Some("project".to_string()),
        );
        memory.id = id.to_string();
        queries::insert_memory(conn, &memory).unwrap();
        memory
    }

    fn link_memory(conn: &Connection, memory: &Memory, gateway_id: &str, revision: i64) {
        queries::upsert_memory_gateway_sync(
            conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: memory.id.clone(),
                project: memory.project.clone().unwrap(),
                gateway_memory_id: gateway_id.to_string(),
                last_seen_server_revision: revision,
                last_pushed_content_hash: Some(memory_content_hash(
                    &memory.content,
                    memory.memory_type.as_deref().unwrap_or("user"),
                    memory.tags.as_deref().unwrap_or(&[]),
                )),
                last_pulled_content_hash: None,
                sync_state: "created".to_string(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn update_pushes_single_memory_and_records_sync_metadata() {
        let conn = open_mem_db();
        let memory = insert_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000301",
            "updated body",
        );
        let (base_url, server) = spawn_gateway(|request| {
            let memory = request["memories"].as_array().unwrap().first().unwrap();
            assert_eq!(memory["content"], json!("updated body"));
            assert!(memory.get("tombstone").is_none());
            json!({
                "project_ident": "agent-memory",
                "server_revision": 11,
                "results": [{
                    "local_memory_id": memory["local_memory_id"],
                    "gateway_memory_id": "gw-update",
                    "server_revision": 11,
                    "action": "created",
                    "content_hash": memory["content_hash"]
                }]
            })
        });

        let outcome =
            push_memory_update_if_configured(&conn, &gateway_config(base_url), &memory).unwrap();

        assert!(outcome.was_synced());
        let sync = queries::get_memory_gateway_sync(&conn, &memory.id)
            .unwrap()
            .unwrap();
        assert_eq!(sync.gateway_memory_id, "gw-update");
        assert_eq!(sync.last_seen_server_revision, 11);
        assert_eq!(sync.sync_state, "created");
        assert!(server.join().unwrap()["memories"][0]["tombstone"].is_null());
    }

    #[test]
    fn tombstone_pushes_delete_for_linked_memory() {
        let conn = open_mem_db();
        let memory = insert_memory(&conn, "aaaaaaaa-0000-1111-2222-000000000302", "delete me");
        link_memory(&conn, &memory, "gw-delete", 8);
        let (base_url, server) = spawn_gateway(|request| {
            let memory = request["memories"].as_array().unwrap().first().unwrap();
            assert_eq!(memory["gateway_memory_id"], json!("gw-delete"));
            assert_eq!(memory["base_gateway_revision"], json!(8));
            assert_eq!(memory["tombstone"], json!(true));
            json!({
                "project_ident": "agent-memory",
                "server_revision": 12,
                "results": [{
                    "local_memory_id": memory["local_memory_id"],
                    "gateway_memory_id": "gw-delete",
                    "server_revision": 12,
                    "action": "tombstoned",
                    "content_hash": memory["content_hash"]
                }]
            })
        });

        let outcome = tombstone_memory_before_local_removal(
            &conn,
            &gateway_config(base_url),
            &memory,
            "test delete",
        )
        .unwrap();

        assert!(outcome.was_synced());
        let sync = queries::get_memory_gateway_sync(&conn, &memory.id)
            .unwrap()
            .expect("tombstone sync row");
        assert_eq!(sync.gateway_memory_id, "gw-delete");
        assert_eq!(sync.last_seen_server_revision, 12);
        assert_eq!(sync.sync_state, "tombstoned");
        assert!(sync.tombstone_deleted);
        assert_eq!(server.join().unwrap()["memories"][0]["tombstone"], true);
    }

    #[test]
    fn tombstone_skips_unlinked_memory_without_network() {
        let conn = open_mem_db();
        let memory = insert_memory(&conn, "aaaaaaaa-0000-1111-2222-000000000303", "local only");
        let config = GatewayConfig {
            base_url: Some("http://127.0.0.1:1".to_string()),
            api_key: Some("test-key".to_string()),
            auto_sync: Some(true),
            okf_sync: None,
        };

        let outcome =
            tombstone_memory_before_local_removal(&conn, &config, &memory, "test delete").unwrap();

        assert_eq!(
            outcome,
            GatewayMutationOutcome::Skipped {
                reason: "memory is not linked to gateway"
            }
        );
    }

    #[test]
    fn tombstone_treats_missing_remote_target_as_idempotent_success() {
        let conn = open_mem_db();
        let memory = insert_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000304",
            "remote row already gone",
        );
        link_memory(&conn, &memory, "gw-missing", 17);
        let (base_url, server) = spawn_gateway(|request| {
            let memory = request["memories"].as_array().unwrap().first().unwrap();
            json!({
                "project_ident": "agent-memory",
                "server_revision": 18,
                "results": [{
                    "local_memory_id": memory["local_memory_id"],
                    "gateway_memory_id": "gw-missing",
                    "action": "rejected",
                    "error": "gateway_memory_id was not found for this project"
                }]
            })
        });

        let outcome = tombstone_memory_before_local_removal(
            &conn,
            &gateway_config(base_url),
            &memory,
            "test delete",
        )
        .unwrap();

        assert_eq!(
            outcome,
            GatewayMutationOutcome::Skipped {
                reason: "gateway tombstone target already absent"
            }
        );
        let sync = queries::get_memory_gateway_sync(&conn, &memory.id)
            .unwrap()
            .expect("sync row retained until local delete");
        assert_eq!(sync.sync_state, "already_absent");
        assert!(sync.tombstone_deleted);
        assert_eq!(server.join().unwrap()["memories"][0]["tombstone"], true);
    }

    #[test]
    fn local_gateway_projection_is_capability_gated() {
        let conn = open_mem_db();
        let memory = insert_memory(&conn, "agent-memory", "body");
        let legacy =
            gateway_memory_from_local(&conn, &memory, "agent-memory", None, false).unwrap();
        assert!(legacy.okf.is_none());
        assert!(legacy.concept_hash.is_none());

        let capable =
            gateway_memory_from_local(&conn, &memory, "agent-memory", None, true).unwrap();
        assert!(capable.okf.is_some());
        assert_eq!(
            capable.concept_hash.as_deref(),
            capable.okf.as_ref().map(|okf| okf.semantic_hash.as_str())
        );
        assert_eq!(legacy.content_hash, capable.content_hash);
    }

    #[test]
    fn absent_update_recreates_only_after_complete_paginated_snapshot() {
        let conn = open_mem_db();
        let old = insert_memory(&conn, "absent", "original body");
        link_memory(&conn, &old, "stale-id", 235);
        queries::update_content(&conn, &old.id, "valuable new body", None, None).unwrap();
        let memory = queries::get_memory_by_id(&conn, &old.id).unwrap();
        let before = local_history(&conn, &old.id);
        let responses = vec![
            rejected(&memory, "gateway_memory_id was not found for this project"),
            json!({"project_ident":"agent-memory", "memories":[], "has_more":true, "next_since_revision":100}),
            json!({"project_ident":"agent-memory", "memories":[], "has_more":false}),
            success(&memory, "new-id", 9, "created"),
        ];
        let (url, server) = scripted_gateway(responses);
        push_memory_update_if_configured(&conn, &gateway_config(url), &memory).unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests[1]["since_revision"], Value::Null);
        assert_eq!(requests[2]["since_revision"], 100);
        assert_eq!(requests[3]["memories"][0]["gateway_memory_id"], Value::Null);
        assert_eq!(
            requests[3]["memories"][0]["base_gateway_revision"],
            Value::Null
        );
        assert_eq!(
            queries::get_memory_gateway_sync(&conn, &old.id)
                .unwrap()
                .unwrap()
                .gateway_memory_id,
            "new-id"
        );
        assert_eq!(local_history(&conn, &old.id), before);
    }

    #[test]
    fn revision_rebase_requires_acknowledged_remote_content_for_updates_and_deletes() {
        for revision in [173, 300] {
            for deleting in [false, true] {
                let conn = open_mem_db();
                let old = insert_memory(&conn, "revision", "acknowledged body");
                link_memory(&conn, &old, "same-id", 235);
                queries::update_content(&conn, &old.id, "local edit", None, None).unwrap();
                let memory = queries::get_memory_by_id(&conn, &old.id).unwrap();
                let before = local_history(&conn, &old.id);
                let responses = vec![
                    conflict(&memory),
                    snapshot(&old, revision, false),
                    success(
                        &memory,
                        "same-id",
                        revision + 1,
                        if deleting { "tombstoned" } else { "updated" },
                    ),
                ];
                let (url, server) = scripted_gateway(responses);
                let config = gateway_config(url);
                if deleting {
                    tombstone_memory_before_local_removal(&conn, &config, &memory, "test").unwrap();
                } else {
                    push_memory_update_if_configured(&conn, &config, &memory).unwrap();
                }
                let requests = server.join().unwrap();
                assert_eq!(
                    requests[2]["memories"][0]["base_gateway_revision"],
                    revision
                );
                assert_eq!(
                    queries::get_memory_gateway_sync(&conn, &old.id)
                        .unwrap()
                        .unwrap()
                        .last_seen_server_revision,
                    revision + 1
                );
                assert_eq!(local_history(&conn, &old.id), before);
            }
        }
    }

    #[test]
    fn remote_behind_retained_base_rebases_exact_history_but_remote_ahead_does_not() {
        for revision in [173, 300] {
            let conn = open_mem_db();
            let old = insert_memory(&conn, "history", "original retained guidance");
            queries::update_content(&conn, &old.id, "acknowledged revision", None, None).unwrap();
            let acknowledged = queries::get_memory_by_id(&conn, &old.id).unwrap();
            link_memory(&conn, &acknowledged, "same-id", 235);
            queries::update_content(&conn, &old.id, "new condensed guidance", None, None).unwrap();
            let memory = queries::get_memory_by_id(&conn, &old.id).unwrap();
            let before = local_history(&conn, &old.id);
            let mut responses = vec![conflict(&memory), snapshot(&old, revision, false)];
            if revision < 235 {
                responses.push(success(&memory, "same-id", revision + 1, "updated"));
            }
            let (url, server) = scripted_gateway(responses);
            let result = push_memory_update_if_configured(&conn, &gateway_config(url), &memory);
            assert_eq!(result.is_ok(), revision < 235);
            assert_eq!(
                server.join().unwrap().len(),
                if revision < 235 { 3 } else { 2 }
            );
            assert_eq!(local_history(&conn, &old.id), before);
        }
    }

    #[test]
    fn divergent_content_and_remote_tombstones_are_never_overwritten_or_resurrected() {
        for revision in [173, 300] {
            for tombstone in [false, true] {
                let conn = open_mem_db();
                let memory = insert_memory(&conn, "protected", "valuable local body");
                link_memory(&conn, &memory, "same-id", 235);
                let mut remote = memory.clone();
                remote.content = "independent remote body".into();
                let before = local_history(&conn, &memory.id);
                let (url, server) = scripted_gateway(vec![
                    conflict(&memory),
                    snapshot(&remote, revision, tombstone),
                ]);
                assert!(
                    push_memory_update_if_configured(&conn, &gateway_config(url), &memory).is_err()
                );
                assert_eq!(server.join().unwrap().len(), 2);
                assert_eq!(
                    queries::get_memory_gateway_sync(&conn, &memory.id)
                        .unwrap()
                        .unwrap()
                        .last_seen_server_revision,
                    235
                );
                assert_eq!(local_history(&conn, &memory.id), before);
            }
        }
    }

    #[test]
    fn remote_tombstone_delete_reconciles_without_a_second_write() {
        let conn = open_mem_db();
        let memory = insert_memory(&conn, "deleted", "valuable local body");
        link_memory(&conn, &memory, "same-id", 103);
        let (url, server) = scripted_gateway(vec![conflict(&memory), snapshot(&memory, 118, true)]);
        tombstone_memory_before_local_removal(&conn, &gateway_config(url), &memory, "test")
            .unwrap();
        let sync = queries::get_memory_gateway_sync(&conn, &memory.id)
            .unwrap()
            .unwrap();
        assert!(sync.tombstone_deleted);
        assert_eq!(sync.last_seen_server_revision, 118);
        assert_eq!(server.join().unwrap().len(), 2);
        assert!(queries::get_memory_by_id(&conn, &memory.id).is_ok());
    }

    #[test]
    fn gateway_identity_guard_preserves_mappings_and_cursors_without_contacting_another_endpoint() {
        let conn = open_mem_db();
        let memory = insert_memory(&conn, "identity", "valuable body");
        link_memory(&conn, &memory, "same-id", 9);
        queries::upsert_project_gateway_sync_state(&conn, "agent-memory", Some(9), Some("cursor"))
            .unwrap();
        let original =
            MemoryGatewayClient::from_parts("https://gateway.example.org/", "key").unwrap();
        ensure_gateway_identity(&conn, &original, false).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM gateway_sync_identity", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "status must not bind state");
        ensure_gateway_identity(&conn, &original, true).unwrap();
        ensure_gateway_identity(
            &conn,
            &MemoryGatewayClient::from_parts("https://gateway.example.org", "new-key").unwrap(),
            true,
        )
        .unwrap();
        assert!(push_memory_update_if_configured(
            &conn,
            &gateway_config("http://127.0.0.1:1".into()),
            &memory
        )
        .unwrap_err()
        .to_string()
        .contains("identity changed"));
        assert_eq!(
            queries::get_memory_gateway_sync(&conn, &memory.id)
                .unwrap()
                .unwrap()
                .last_seen_server_revision,
            9
        );
        assert_eq!(
            queries::get_project_gateway_sync_state(&conn, "agent-memory")
                .unwrap()
                .unwrap()
                .last_pull_cursor
                .as_deref(),
            Some("cursor")
        );
    }

    #[test]
    fn global_reference_uses_authorized_wire_type_and_keeps_local_and_okf_semantics() {
        let conn = open_mem_db();
        let memory = Memory::new(
            "Reference guidance".into(),
            None,
            Some("__global__".into()),
            None,
            None,
            Some("reference".into()),
        );
        queries::insert_memory(&conn, &memory).unwrap();
        let wire = gateway_memory_from_local(&conn, &memory, "__global__", None, true).unwrap();
        assert_eq!(wire.memory_type, "global");
        assert!(wire.okf.unwrap().document.contains("reference"));
        assert_eq!(
            queries::get_memory_by_id(&conn, &memory.id)
                .unwrap()
                .memory_type
                .as_deref(),
            Some("reference")
        );
        let mut response = success(&memory, "global-id", 1, "created");
        response["project_ident"] = json!("__global__");
        let (url, server) = scripted_gateway(vec![response]);
        push_memory_update_if_configured(&conn, &gateway_config(url), &memory).unwrap();
        assert_eq!(
            server.join().unwrap()[0]["memories"][0]["memory_type"],
            "global"
        );
        assert_eq!(
            queries::get_memory_by_id(&conn, &memory.id)
                .unwrap()
                .memory_type
                .as_deref(),
            Some("reference")
        );
    }

    fn local_history(conn: &Connection, id: &str) -> (String, Option<String>, i64) {
        conn.query_row("SELECT content, content_raw, current_revision FROM memories JOIN memory_concepts ON memories.id = memory_id WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap()
    }
    fn rejected(memory: &Memory, reason: &str) -> Value {
        json!({"project_ident":"agent-memory", "results":[{"local_memory_id":memory.id,"gateway_memory_id":"same-id","action":"rejected","error":reason}]})
    }
    fn conflict(memory: &Memory) -> Value {
        json!({"project_ident":"agent-memory", "results":[{"local_memory_id":memory.id,"gateway_memory_id":"same-id","action":"conflict","conflict":{"reason":"base revision mismatch"}}]})
    }
    fn success(memory: &Memory, id: &str, revision: i64, action: &str) -> Value {
        json!({"project_ident":"agent-memory", "results":[{"local_memory_id":memory.id,"gateway_memory_id":id,"server_revision":revision,"action":action}]})
    }
    fn snapshot(memory: &Memory, revision: i64, tombstone: bool) -> Value {
        let hash = memory_content_hash(&memory.content, "project", &[]);
        if tombstone {
            json!({"project_ident":"agent-memory", "tombstones":[{"project_ident":"agent-memory","gateway_memory_id":"same-id","server_revision":revision,"content_hash":hash}]})
        } else {
            // Actual server shape uses `id`, not `gateway_memory_id`.
            json!({"project_ident":"agent-memory", "memories":[{"project_ident":"agent-memory","id":"same-id","server_revision":revision,"content":memory.content,"content_hash":hash,"memory_type":"project","tags":memory.tags.clone().unwrap_or_default()}]})
        }
    }
    fn scripted_gateway(responses: Vec<Value>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut buf = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 4096];
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..header_end]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                while buf.len() < header_end + length {
                    let mut chunk = [0u8; 4096];
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&chunk[..n]);
                }
                requests
                    .push(serde_json::from_slice(&buf[header_end..header_end + length]).unwrap());
                let body = response.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
            requests
        });
        (format!("http://{addr}"), server)
    }

    fn spawn_gateway<F>(handler: F) -> (String, std::thread::JoinHandle<Value>)
    where
        F: FnOnce(Value) -> Value + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind gateway");
        let addr = listener.local_addr().expect("gateway addr");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            let mut buf = [0_u8; 16 * 1024];
            let n = stream.read(&mut buf).expect("read request");
            let request = String::from_utf8_lossy(&buf[..n]);
            let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
            let value: Value = serde_json::from_str(body).expect("request json");
            let response = handler(value.clone());
            let body = response.to_string();
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(http.as_bytes()).expect("write response");
            value
        });
        (format!("http://{addr}"), handle)
    }
}
