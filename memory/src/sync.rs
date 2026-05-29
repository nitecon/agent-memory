use chrono::{TimeZone, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::config::GatewayConfig;
use crate::db::queries::GLOBAL_PROJECT_IDENT;

pub const PUSH_MEMORIES_PATH: &str = "/v1/projects/{project}/memories/push";
pub const PULL_MEMORIES_PATH: &str = "/v1/projects/{project}/memories/pull";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemorySyncValidationError {
    EmptyProject,
    GlobalProjectExcluded,
    ProjectMismatch { expected: String, actual: String },
    WorkingContextExcluded,
}

impl std::fmt::Display for MemorySyncValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyProject => write!(f, "project ident must not be empty"),
            Self::GlobalProjectExcluded => {
                write!(f, "global memories are excluded from gateway exchange")
            }
            Self::ProjectMismatch { expected, actual } => write!(
                f,
                "memory project ident mismatch: expected {expected}, got {actual}"
            ),
            Self::WorkingContextExcluded => {
                write!(f, "WorkingContext is excluded from gateway exchange")
            }
        }
    }
}

impl std::error::Error for MemorySyncValidationError {}

#[derive(Debug, Error)]
pub enum GatewaySyncClientError {
    #[error("gateway memory exchange is not configured: {0}")]
    Config(String),
    #[error("gateway authentication failed: {0}")]
    Authentication(String),
    #[error("gateway project authorization failed: {0}")]
    ProjectAuthorization(String),
    #[error("gateway validation failed: {0}")]
    Validation(String),
    #[error("gateway transient failure: {0}")]
    Transient(String),
    #[error("gateway transport failed: {0}")]
    Transport(String),
    #[error("gateway response was malformed: {0}")]
    MalformedResponse(String),
    #[error("gateway memory scope validation failed: {0}")]
    Scope(#[from] MemorySyncValidationError),
}

#[derive(Debug, Clone)]
pub struct MemoryGatewayClient {
    base_url: String,
    api_key: String,
    http: reqwest::blocking::Client,
}

impl MemoryGatewayClient {
    pub fn from_config(config: &GatewayConfig) -> Result<Self, GatewaySyncClientError> {
        let base_url = config.base_url.as_deref().ok_or_else(|| {
            GatewaySyncClientError::Config(
                "run `memory setup gateway` (or `agent-tools setup gateway`) or set AGENT_MEMORY_GATEWAY_URL, AGENT_GATEWAY_URL, or GATEWAY_URL".to_string(),
            )
        })?;
        let api_key = config.api_key.as_deref().ok_or_else(|| {
            GatewaySyncClientError::Config(
                "run `memory setup gateway` (or `agent-tools setup gateway`) or set AGENT_MEMORY_GATEWAY_API_KEY, AGENT_GATEWAY_API_KEY, or GATEWAY_API_KEY".to_string(),
            )
        })?;
        Self::from_parts(base_url, api_key)
    }

    pub fn from_parts(base_url: &str, api_key: &str) -> Result<Self, GatewaySyncClientError> {
        let base_url = normalize_base_url(base_url)?;
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(GatewaySyncClientError::Config(
                "gateway API key must not be empty".to_string(),
            ));
        }
        Ok(Self {
            base_url,
            api_key: api_key.to_string(),
            http: reqwest::blocking::Client::new(),
        })
    }

    pub fn push_memories(
        &self,
        request: &PushMemoriesRequest,
    ) -> Result<PushMemoriesResponse, GatewaySyncClientError> {
        request.validate_project_scope()?;
        self.post_json(
            &project_memory_path(PUSH_MEMORIES_PATH, &request.project),
            request,
        )
    }

    pub fn pull_memories(
        &self,
        request: &PullMemoriesRequest,
    ) -> Result<PullMemoriesResponse, GatewaySyncClientError> {
        request.validate_project_scope()?;
        let mut response: PullMemoriesResponse = self.post_json(
            &project_memory_path(PULL_MEMORIES_PATH, &request.project),
            request,
        )?;
        response.normalize_gateway_shape();
        Ok(response)
    }

    fn post_json<T, R>(&self, path: &str, body: &T) -> Result<R, GatewaySyncClientError>
    where
        T: Serialize,
        R: DeserializeOwned,
    {
        let url = self.endpoint(path);
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .map_err(|err| GatewaySyncClientError::Transport(err.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .map_err(|err| GatewaySyncClientError::Transport(err.to_string()))?;
        if !status.is_success() {
            return Err(classify_gateway_status(status, &text));
        }
        serde_json::from_str(&text)
            .map_err(|err| GatewaySyncClientError::MalformedResponse(err.to_string()))
    }

    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, normalize_path(path))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushMemoriesRequest {
    #[serde(skip_serializing)]
    pub project: String,
    pub memories: Vec<GatewayMemory>,
}

impl PushMemoriesRequest {
    pub fn validate_project_scope(&self) -> Result<(), MemorySyncValidationError> {
        validate_project_ident(&self.project)?;
        for memory in &self.memories {
            memory.validate_project_scope(&self.project)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushMemoriesResponse {
    #[serde(alias = "project_ident")]
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_revision: Option<i64>,
    pub results: Vec<PushMemoryResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushMemoryResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_memory_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_memory_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_revision: Option<i64>,
    pub action: PushMemoryAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<MemoryConflict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<MemoryValidationError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushMemoryAction {
    Created,
    Updated,
    Linked,
    Conflict,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullMemoriesRequest {
    #[serde(skip_serializing)]
    pub project: String,
    #[serde(
        rename = "since_revision",
        alias = "since_server_revision",
        skip_serializing_if = "Option::is_none"
    )]
    pub since_server_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(
        default,
        rename = "known",
        alias = "known_memories",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub known_memories: Vec<KnownGatewayMemory>,
    #[serde(
        rename = "page_size",
        alias = "limit",
        skip_serializing_if = "Option::is_none"
    )]
    pub limit: Option<u32>,
}

impl PullMemoriesRequest {
    pub fn validate_project_scope(&self) -> Result<(), MemorySyncValidationError> {
        validate_project_ident(&self.project)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullMemoriesResponse {
    #[serde(alias = "project_ident")]
    pub project: String,
    #[serde(default)]
    pub memories: Vec<GatewayMemory>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tombstones: Vec<GatewayMemoryTombstoneRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_revision: Option<i64>,
    #[serde(default)]
    pub has_more: bool,
}

impl PullMemoriesResponse {
    fn normalize_gateway_shape(&mut self) {
        let tombstones = std::mem::take(&mut self.tombstones);
        self.memories.extend(
            tombstones
                .into_iter()
                .map(GatewayMemoryTombstoneRecord::into_memory),
        );
    }

    pub fn validate_project_scope(&self) -> Result<(), MemorySyncValidationError> {
        validate_project_ident(&self.project)?;
        for memory in &self.memories {
            memory.validate_project_scope(&self.project)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownGatewayMemory {
    pub gateway_memory_id: String,
    pub server_revision: i64,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayMemory {
    #[serde(rename = "project_ident", alias = "project")]
    pub project: String,
    pub content: String,
    pub memory_type: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub content_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_memory_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_memory_id: Option<String>,
    #[serde(
        rename = "base_gateway_revision",
        alias = "base_server_revision",
        skip_serializing_if = "Option::is_none"
    )]
    pub base_server_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_revision: Option<i64>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_timestamp",
        skip_serializing_if = "Option::is_none"
    )]
    pub created_at: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_timestamp",
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<GatewayMemoryProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<GatewayMemoryTombstone>,
}

impl GatewayMemory {
    pub fn validate_project_scope(
        &self,
        expected_project: &str,
    ) -> Result<(), MemorySyncValidationError> {
        validate_project_ident(expected_project)?;
        validate_project_ident(&self.project)?;
        if self.memory_type == "working_context" {
            return Err(MemorySyncValidationError::WorkingContextExcluded);
        }
        if self.project != expected_project {
            return Err(MemorySyncValidationError::ProjectMismatch {
                expected: expected_project.to_string(),
                actual: self.project.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayMemoryTombstoneRecord {
    #[serde(rename = "project_ident", alias = "project")]
    pub project: String,
    pub gateway_memory_id: String,
    pub server_revision: i64,
    pub content_hash: String,
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub tombstoned_at: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub updated_at: Option<String>,
}

impl GatewayMemoryTombstoneRecord {
    fn into_memory(self) -> GatewayMemory {
        GatewayMemory {
            project: self.project,
            content: String::new(),
            memory_type: "project".to_string(),
            tags: Vec::new(),
            content_hash: self.content_hash,
            local_memory_id: None,
            client_id: None,
            gateway_memory_id: Some(self.gateway_memory_id),
            base_server_revision: None,
            server_revision: Some(self.server_revision),
            created_at: None,
            updated_at: self.updated_at.clone(),
            provenance: None,
            tombstone: Some(GatewayMemoryTombstone {
                deleted: true,
                deleted_at: self.tombstoned_at.or(self.updated_at),
                reason: Some("remote memory deleted".to_string()),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayMemoryProvenance {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_machine_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pushed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayMemoryTombstone {
    pub deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConflict {
    #[serde(
        rename = "base_gateway_revision",
        alias = "base_server_revision",
        skip_serializing_if = "Option::is_none"
    )]
    pub base_server_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_server_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_content_hash: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryValidationError {
    pub code: String,
    pub message: String,
}

fn deserialize_optional_timestamp<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(serde_json::Value::Number(value)) => {
            let millis = value
                .as_i64()
                .ok_or_else(|| serde::de::Error::custom("timestamp must be an integer"))?;
            let timestamp = Utc
                .timestamp_millis_opt(millis)
                .single()
                .ok_or_else(|| serde::de::Error::custom("timestamp is out of range"))?;
            Ok(Some(timestamp.to_rfc3339()))
        }
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected timestamp string or integer, got {other}"
        ))),
    }
}

fn validate_project_ident(project: &str) -> Result<(), MemorySyncValidationError> {
    if project.trim().is_empty() {
        return Err(MemorySyncValidationError::EmptyProject);
    }
    if project == GLOBAL_PROJECT_IDENT {
        return Err(MemorySyncValidationError::GlobalProjectExcluded);
    }
    Ok(())
}

pub fn memory_content_hash(content: &str, memory_type: &str, tags: &[String]) -> String {
    let _ = (memory_type, tags);

    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn normalize_base_url(base_url: &str) -> Result<String, GatewaySyncClientError> {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(GatewaySyncClientError::Config(
            "gateway base URL must not be empty".to_string(),
        ));
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(GatewaySyncClientError::Config(
            "gateway base URL must start with http:// or https://".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn project_memory_path(template: &str, project: &str) -> String {
    template.replace("{project}", &encode_path_segment(project))
}

fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::new();
    for byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&hex_upper(*byte));
        }
    }
    encoded
}

fn hex_upper(byte: u8) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(2);
    out.push(HEX[(byte >> 4) as usize] as char);
    out.push(HEX[(byte & 0x0f) as usize] as char);
    out
}

fn classify_gateway_status(status: reqwest::StatusCode, body: &str) -> GatewaySyncClientError {
    let message = body.trim();
    let message = if message.is_empty() {
        status.to_string()
    } else {
        message.to_string()
    };

    match status.as_u16() {
        401 => GatewaySyncClientError::Authentication(message),
        403 => GatewaySyncClientError::ProjectAuthorization(message),
        400 | 409 | 422 => GatewaySyncClientError::Validation(message),
        500..=599 => GatewaySyncClientError::Transient(message),
        _ => GatewaySyncClientError::Transport(format!("{status}: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn client_normalizes_endpoint_paths() {
        let client = MemoryGatewayClient::from_parts(" https://gateway.example/ ", "key").unwrap();
        assert_eq!(
            client.endpoint(PUSH_MEMORIES_PATH),
            "https://gateway.example/v1/projects/{project}/memories/push"
        );
        assert_eq!(
            client.endpoint("v1/projects/{project}/memories/pull"),
            "https://gateway.example/v1/projects/{project}/memories/pull"
        );
    }

    #[test]
    fn project_memory_paths_encode_project_ident_as_path_segment() {
        assert_eq!(
            project_memory_path(PUSH_MEMORIES_PATH, "agent-memory"),
            "/v1/projects/agent-memory/memories/push"
        );
        assert_eq!(
            project_memory_path(PULL_MEMORIES_PATH, "org/repo with space"),
            "/v1/projects/org%2Frepo%20with%20space/memories/pull"
        );
    }

    #[test]
    fn client_rejects_missing_scheme_and_key() {
        let bad_url = MemoryGatewayClient::from_parts("gateway.example", "key").unwrap_err();
        assert!(bad_url.to_string().contains("must start with"));

        let bad_key = MemoryGatewayClient::from_parts("https://gateway.example", " ").unwrap_err();
        assert!(bad_key.to_string().contains("API key must not be empty"));
    }

    #[test]
    fn from_config_requires_url_and_key() {
        let missing = GatewayConfig::default();
        let err = MemoryGatewayClient::from_config(&missing).unwrap_err();
        assert!(err.to_string().contains("AGENT_MEMORY_GATEWAY_URL"));

        let missing_key = GatewayConfig {
            base_url: Some("https://gateway.example".to_string()),
            api_key: None,
        };
        let err = MemoryGatewayClient::from_config(&missing_key).unwrap_err();
        assert!(err.to_string().contains("AGENT_MEMORY_GATEWAY_API_KEY"));
    }

    #[test]
    fn classifies_gateway_http_statuses() {
        assert!(matches!(
            classify_gateway_status(StatusCode::UNAUTHORIZED, "nope"),
            GatewaySyncClientError::Authentication(_)
        ));
        assert!(matches!(
            classify_gateway_status(StatusCode::FORBIDDEN, "denied"),
            GatewaySyncClientError::ProjectAuthorization(_)
        ));
        assert!(matches!(
            classify_gateway_status(StatusCode::UNPROCESSABLE_ENTITY, "bad memory"),
            GatewaySyncClientError::Validation(_)
        ));
        assert!(matches!(
            classify_gateway_status(StatusCode::INTERNAL_SERVER_ERROR, "down"),
            GatewaySyncClientError::Transient(_)
        ));
    }

    #[test]
    fn content_hash_is_stable_across_tag_order() {
        let a = memory_content_hash(
            "body",
            "project",
            &["sre".to_string(), "gateway".to_string()],
        );
        let b = memory_content_hash(
            "body",
            "project",
            &["gateway".to_string(), "sre".to_string()],
        );
        assert_eq!(a, b);
        assert_eq!(
            a,
            "230d8358dc8e8890b4c58deeb62912ee2f20357ae92a5cc861b98e68fe31acb5"
        );
    }
}
