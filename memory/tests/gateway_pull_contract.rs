use agent_memory::sync::{
    GatewayMemory, GatewayMemoryProvenance, GatewayMemoryTombstone, KnownGatewayMemory,
    PullMemoriesRequest, PullMemoriesResponse,
};
use serde_json::json;

fn remote_memory() -> GatewayMemory {
    GatewayMemory {
        project: "agent-memory".to_string(),
        content: "Remote project memory".to_string(),
        memory_type: "project".to_string(),
        tags: vec!["sre".to_string(), "gateway-sync".to_string()],
        content_hash: "sha256:remote".to_string(),
        local_memory_id: None,
        client_id: None,
        gateway_memory_id: Some("gw-1".to_string()),
        base_server_revision: None,
        server_revision: Some(42),
        created_at: Some("2026-05-29T13:00:00Z".to_string()),
        updated_at: Some("2026-05-29T13:30:00Z".to_string()),
        provenance: Some(GatewayMemoryProvenance {
            source_agent_id: Some("sre-agent".to_string()),
            source_machine_id: Some("infra-host".to_string()),
            source_system: Some("agent-memory".to_string()),
            pushed_at: Some("2026-05-29T13:30:00Z".to_string()),
        }),
        tombstone: None,
    }
}

#[test]
fn pull_request_is_project_plus_cursor_state() {
    let request = PullMemoriesRequest {
        project: "agent-memory".to_string(),
        since_server_revision: Some(41),
        cursor: Some("cursor-1".to_string()),
        known_memories: vec![KnownGatewayMemory {
            gateway_memory_id: "gw-1".to_string(),
            server_revision: 41,
            content_hash: "sha256:old".to_string(),
        }],
        limit: Some(100),
    };

    request.validate_project_scope().unwrap();

    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        value,
        json!({
            "project": "agent-memory",
            "since_server_revision": 41,
            "cursor": "cursor-1",
            "known_memories": [
                {
                    "gateway_memory_id": "gw-1",
                    "server_revision": 41,
                    "content_hash": "sha256:old"
                }
            ],
            "limit": 100
        })
    );
}

#[test]
fn pull_response_returns_memory_array_and_cursor() {
    let response = PullMemoriesResponse {
        project: "agent-memory".to_string(),
        memories: vec![remote_memory()],
        next_cursor: Some("cursor-2".to_string()),
        server_revision: Some(42),
        has_more: true,
    };

    response.validate_project_scope().unwrap();

    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["project"], "agent-memory");
    assert_eq!(value["next_cursor"], "cursor-2");
    assert_eq!(value["server_revision"], 42);
    assert_eq!(value["has_more"], true);
    assert_eq!(value["memories"][0]["gateway_memory_id"], "gw-1");
    assert_eq!(
        value["memories"][0]["provenance"]["source_agent_id"],
        "sre-agent"
    );
}

#[test]
fn pull_response_carries_tombstones_without_content_deletion_policy() {
    let mut memory = remote_memory();
    memory.content = "".to_string();
    memory.content_hash = "sha256:tombstone".to_string();
    memory.tombstone = Some(GatewayMemoryTombstone {
        deleted: true,
        deleted_at: Some("2026-05-29T13:45:00Z".to_string()),
        reason: Some("remote memory deleted".to_string()),
    });

    let response = PullMemoriesResponse {
        project: "agent-memory".to_string(),
        memories: vec![memory],
        next_cursor: None,
        server_revision: Some(43),
        has_more: false,
    };

    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["memories"][0]["tombstone"]["deleted"], true);
    assert_eq!(
        value["memories"][0]["tombstone"]["reason"],
        "remote memory deleted"
    );
}

#[test]
fn pull_response_rejects_malformed_project() {
    let mut memory = remote_memory();
    memory.project = "other-project".to_string();
    let response = PullMemoriesResponse {
        project: "agent-memory".to_string(),
        memories: vec![memory],
        next_cursor: None,
        server_revision: Some(42),
        has_more: false,
    };

    let err = response.validate_project_scope().unwrap_err();
    assert_eq!(
        err.to_string(),
        "memory project ident mismatch: expected agent-memory, got other-project"
    );
}
