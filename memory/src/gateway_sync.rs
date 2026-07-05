use std::env;

use rusqlite::Connection;

use crate::config::GatewayConfig;
use crate::db::models::{Memory, MemoryGatewaySync, MemoryGatewaySyncUpsert};
use crate::db::queries;
use crate::error::MemoryError;
use crate::sync::{
    memory_content_hash, GatewayMemory, GatewayMemoryProvenance, GatewayMemoryTombstone,
    GatewaySyncClientError, MemoryGatewayClient, PushMemoriesRequest, PushMemoryAction,
    PushMemoryResult,
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
    let gateway_memory = gateway_memory_from_local(memory, project, sync.as_ref())?;
    let content_hash = gateway_memory.content_hash.clone();
    let result = push_single(gateway_config, gateway_memory)?;

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
    let Some(sync) = queries::get_memory_gateway_sync(conn, &memory.id)? else {
        return Ok(GatewayMutationOutcome::Skipped {
            reason: "memory is not linked to gateway",
        });
    };
    if sync.tombstone_deleted {
        return Ok(GatewayMutationOutcome::Skipped {
            reason: "gateway tombstone already recorded",
        });
    }

    let gateway_memory = gateway_tombstone_from_local(memory, project, &sync, reason)?;
    let content_hash = gateway_memory.content_hash.clone();
    let result = push_single(gateway_config, gateway_memory)?;

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
            Err(push_result_error("gateway tombstone", &result))
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
    memory: &Memory,
    project: &str,
    sync: Option<&MemoryGatewaySync>,
) -> Result<GatewayMemory, MemoryError> {
    if memory.project.as_deref() != Some(project) {
        return Err(MemoryError::Config(format!(
            "memory {} is not scoped to project {project}",
            memory.id
        )));
    }
    let memory_type = memory
        .memory_type
        .clone()
        .unwrap_or_else(|| "user".to_string());
    let tags = memory.tags.clone().unwrap_or_default();
    let content_hash = memory_content_hash(&memory.content, &memory_type, &tags);
    Ok(GatewayMemory {
        project: project.to_string(),
        content: memory.content.clone(),
        memory_type,
        tags,
        content_hash,
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
    memory: &Memory,
    project: &str,
    sync: &MemoryGatewaySync,
    reason: &str,
) -> Result<GatewayMemory, MemoryError> {
    let mut gateway_memory = gateway_memory_from_local(memory, project, Some(sync))?;
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
        memory_type: "project".to_string(),
        tags: Vec::new(),
        content_hash,
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
    let response = MemoryGatewayClient::from_config(gateway_config)
        .map_err(map_gateway_error)?
        .push_memories(&request)
        .map_err(map_gateway_error)?;

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
    if results.len() == 1 {
        return Ok(results.remove(0));
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
