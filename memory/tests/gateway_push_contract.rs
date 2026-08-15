use agent_memory::sync::{
    GatewayMemory, GatewayOkfEnvelope, MemoryConflict, MemoryValidationError, PushMemoriesRequest,
    PushMemoriesResponse, PushMemoryAction, PushMemoryResult,
};
use serde_json::json;

fn project_memory() -> GatewayMemory {
    GatewayMemory {
        project: "agent-memory".to_string(),
        content: "Project-only memory body".to_string(),
        memory_type: "project".to_string(),
        tags: vec!["gateway-sync".to_string(), "project-ident".to_string()],
        content_hash: "abc123".to_string(),
        concept_hash: None,
        okf: None,
        local_memory_id: Some("local-1".to_string()),
        client_id: Some("client-1".to_string()),
        gateway_memory_id: None,
        base_server_revision: None,
        server_revision: None,
        created_at: Some("1970-01-01T00:00:01Z".to_string()),
        updated_at: Some("1970-01-01T00:00:02Z".to_string()),
        provenance: None,
        tombstone: None,
    }
}

#[test]
fn push_request_is_project_plus_memory_array() {
    let request = PushMemoriesRequest {
        project: "agent-memory".to_string(),
        memories: vec![project_memory()],
    };

    request.validate_project_scope().unwrap();

    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        value,
        json!({
            "memories": [
                {
                    "project_ident": "agent-memory",
                    "content": "Project-only memory body",
                    "memory_type": "project",
                    "tags": ["gateway-sync", "project-ident"],
                    "content_hash": "abc123",
                    "local_memory_id": "local-1",
                    "client_id": "client-1",
                    "created_at": 1000_i64,
                    "updated_at": 2000_i64
                }
            ]
        })
    );
}

#[test]
fn old_gateway_payload_omits_optional_okf_contract() {
    let value = serde_json::to_value(project_memory()).unwrap();
    assert!(value.get("okf").is_none());
    assert!(value.get("concept_hash").is_none());
}

#[test]
fn capable_gateway_payload_carries_versioned_text_envelope_and_both_hashes() {
    let mut memory = project_memory();
    memory.concept_hash = Some("semantic-123".to_string());
    memory.okf = Some(GatewayOkfEnvelope {
        version: 1,
        format: "okf-markdown".to_string(),
        revision: 4,
        semantic_hash: "semantic-123".to_string(),
        document: "---\ntype: Agent Memory/project\n---\nbody\n".to_string(),
        extensions: [("x-gateway".to_string(), json!({"keep": true}))]
            .into_iter()
            .collect(),
    });
    let value = serde_json::to_value(memory).unwrap();
    assert_eq!(value["content_hash"], "abc123");
    assert_eq!(value["concept_hash"], "semantic-123");
    assert_eq!(value["okf"]["version"], 1);
    assert_eq!(value["okf"]["x-gateway"]["keep"], true);
}

#[test]
fn push_request_accepts_global_project() {
    let mut memory = project_memory();
    memory.project = "__global__".to_string();
    let request = PushMemoriesRequest {
        project: "__global__".to_string(),
        memories: vec![memory],
    };

    request.validate_project_scope().unwrap();
}

#[test]
fn push_request_rejects_working_context_memory_type() {
    let mut memory = project_memory();
    memory.memory_type = "working_context".to_string();
    let request = PushMemoriesRequest {
        project: "agent-memory".to_string(),
        memories: vec![memory],
    };

    let err = request.validate_project_scope().unwrap_err();
    assert_eq!(
        err.to_string(),
        "WorkingContext is excluded from gateway exchange"
    );
}

#[test]
fn push_response_covers_created_linked_conflict_and_rejected() {
    let response = PushMemoriesResponse {
        project: "agent-memory".to_string(),
        server_revision: Some(7),
        results: vec![
            PushMemoryResult {
                local_memory_id: Some("local-1".to_string()),
                client_id: None,
                gateway_memory_id: Some("gw-1".to_string()),
                server_revision: Some(1),
                action: PushMemoryAction::Created,
                content_hash: Some("abc123".to_string()),
                conflict: None,
                error: None,
                errors: vec![],
            },
            PushMemoryResult {
                local_memory_id: Some("local-2".to_string()),
                client_id: None,
                gateway_memory_id: Some("gw-1".to_string()),
                server_revision: Some(1),
                action: PushMemoryAction::Linked,
                content_hash: Some("abc123".to_string()),
                conflict: None,
                error: None,
                errors: vec![],
            },
            PushMemoryResult {
                local_memory_id: Some("local-3".to_string()),
                client_id: None,
                gateway_memory_id: Some("gw-3".to_string()),
                server_revision: Some(7),
                action: PushMemoryAction::Conflict,
                content_hash: None,
                conflict: Some(MemoryConflict {
                    base_server_revision: Some(5),
                    remote_server_revision: Some(7),
                    local_content_hash: Some("local".to_string()),
                    remote_content_hash: Some("remote".to_string()),
                    local_concept_hash: None,
                    remote_concept_hash: None,
                    reason: "remote revision moved".to_string(),
                }),
                error: None,
                errors: vec![],
            },
            PushMemoryResult {
                local_memory_id: Some("local-4".to_string()),
                client_id: None,
                gateway_memory_id: None,
                server_revision: None,
                action: PushMemoryAction::Rejected,
                content_hash: None,
                conflict: None,
                error: Some("memory content failed redaction policy".to_string()),
                errors: vec![MemoryValidationError {
                    code: "secret_detected".to_string(),
                    message: "memory content failed redaction policy".to_string(),
                }],
            },
        ],
    };

    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["results"][0]["action"], "created");
    assert_eq!(value["results"][1]["action"], "linked");
    assert_eq!(value["results"][2]["action"], "conflict");
    assert_eq!(value["results"][3]["action"], "rejected");
    assert_eq!(value["results"][3]["errors"][0]["code"], "secret_detected");
}
