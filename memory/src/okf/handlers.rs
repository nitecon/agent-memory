use std::collections::{BTreeMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    diff_concepts, parse_document, render_document, AgentMemoryMetadata, Extensions, OkfAttester,
    OkfConcept, OkfError, OkfExecutor, OkfGenerated, OkfParameter, OkfRelationship, OkfSource,
    OkfStatus, OkfVerification, ParsedDocument, RenderMode, SemanticDiff, UsageWindow,
};
use crate::concepts;
use crate::db::{models::Memory, queries};
use crate::error::MemoryError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleScope {
    Project(String),
    Global,
    Unscoped,
}

impl BundleScope {
    pub fn parse_uri(uri: &str) -> Result<Self, HandlerError> {
        if uri == "okf+memory://global/" || uri == "okf+memory://global" {
            return Ok(Self::Global);
        }
        if uri == "okf+memory://unscoped/" || uri == "okf+memory://unscoped" {
            return Ok(Self::Unscoped);
        }
        if let Some(project) = uri
            .strip_prefix("okf+memory://project/")
            .map(|value| value.trim_end_matches('/'))
            .filter(|value| !value.is_empty() && !value.contains('/'))
        {
            return Ok(Self::Project(percent_decode(project)?));
        }
        Err(HandlerError::InvalidTarget(uri.to_string()))
    }
    pub fn uri(&self) -> String {
        match self {
            Self::Project(project) => {
                format!("okf+memory://project/{}/", percent_encode(project))
            }
            Self::Global => "okf+memory://global/".to_string(),
            Self::Unscoped => "okf+memory://unscoped/".to_string(),
        }
    }

    pub(crate) fn project(&self) -> Option<&str> {
        match self {
            Self::Project(project) => Some(project),
            Self::Global => Some(queries::GLOBAL_PROJECT_IDENT),
            Self::Unscoped => None,
        }
    }

    pub(crate) fn scope_name(&self) -> &'static str {
        match self {
            Self::Project(_) => "project",
            Self::Global => "global",
            Self::Unscoped => "unscoped",
        }
    }

    pub(crate) fn contains_project(&self, project: Option<&str>) -> bool {
        self.project() == project
    }
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

fn percent_decode(value: &str) -> Result<String, HandlerError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(HandlerError::InvalidTarget(value.to_string()));
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|_| HandlerError::InvalidTarget(value.to_string()))?;
            decoded.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| HandlerError::InvalidTarget(value.to_string()))?,
            );
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| HandlerError::InvalidTarget(value.to_string()))
}

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Okf(#[from] OkfError),
    #[error("invalid OKF target: {0}")]
    InvalidTarget(String),
    #[error("virtual path is read-only: {0}")]
    ReadOnly(String),
    #[error("target ID `{target}` does not match document ID `{document}`")]
    IdMismatch { target: String, document: String },
    #[error("memory `{id}` is outside bundle `{bundle}`")]
    ScopeMismatch { id: String, bundle: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedDocument {
    pub id: String,
    pub revision: i64,
    pub virtual_path: String,
    pub content_hash: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutResult {
    pub id: String,
    pub revision: i64,
    pub created: bool,
    pub changed: bool,
    pub dry_run: bool,
    pub diff: SemanticDiff,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredConceptExtras {
    #[serde(default)]
    extensions: Extensions,
    usage_window: Option<UsageWindow>,
    runtime: Option<String>,
    #[serde(default)]
    parameters: Vec<OkfParameter>,
    computation: Option<String>,
    executor: Option<OkfExecutor>,
    attester: Option<OkfAttester>,
    #[serde(default)]
    generated_extensions: Extensions,
    #[serde(default)]
    agent_extensions: Extensions,
}

struct StoredConceptRow {
    concept_type: String,
    title: Option<String>,
    description: Option<String>,
    resource: Option<String>,
    status: String,
    stale_after: Option<String>,
    generated_by: Option<String>,
    generated_at: Option<String>,
    extras_json: String,
    raw_frontmatter: Option<String>,
    revision: i64,
}

pub struct OkfDocumentHandler<'a> {
    conn: &'a Connection,
    scope: BundleScope,
}

impl<'a> OkfDocumentHandler<'a> {
    pub fn new(conn: &'a Connection, scope: BundleScope) -> Self {
        Self { conn, scope }
    }

    pub fn scope(&self) -> &BundleScope {
        &self.scope
    }

    pub fn parse(&self, document_text: &str) -> Result<ParsedDocument, HandlerError> {
        Ok(parse_document(document_text)?)
    }

    pub fn validate(&self, document_text: &str) -> Result<ParsedDocument, HandlerError> {
        self.parse(document_text)
    }

    pub fn render(&self, target: &str) -> Result<RenderedDocument, HandlerError> {
        let id = target_id(target)?;
        let concept = self.load_scoped(&id)?;
        let revision = concepts::current_revision(self.conn, &id)?;
        let text = render_document(&concept, RenderMode::Normalized)?;
        let (virtual_path, content_hash) = self.conn.query_row(
            "SELECT c.virtual_path, r.content_hash
             FROM memory_concepts c JOIN memory_revisions r
               ON r.memory_id = c.memory_id AND r.revision = c.current_revision
             WHERE c.memory_id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(RenderedDocument {
            id,
            revision,
            virtual_path,
            content_hash,
            text,
        })
    }

    pub fn put(
        &self,
        target: Option<&str>,
        parsed: &ParsedDocument,
        expected_revision: Option<i64>,
        dry_run: bool,
    ) -> Result<PutResult, HandlerError> {
        self.put_with_operation(target, parsed, expected_revision, dry_run, "put")
    }

    pub fn put_with_operation(
        &self,
        target: Option<&str>,
        parsed: &ParsedDocument,
        expected_revision: Option<i64>,
        dry_run: bool,
        operation: &str,
    ) -> Result<PutResult, HandlerError> {
        if let Some(path) = target {
            ensure_writable_target(path)?;
            self.validate_target_scope(path)?;
        }
        self.validate_document_scope(&parsed.concept)?;
        let target_id = target.map(target_id).transpose()?;
        let document_id = parsed
            .concept
            .agent_memory
            .as_ref()
            .and_then(|metadata| metadata.id.clone());
        if let (Some(target), Some(document)) = (&target_id, &document_id) {
            if target != document {
                return Err(HandlerError::IdMismatch {
                    target: target.clone(),
                    document: document.clone(),
                });
            }
        }
        let id = target_id
            .or(document_id)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let existing = self
            .conn
            .query_row(
                "SELECT project FROM memories WHERE id = ?1",
                params![id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;

        match existing {
            Some(project) => {
                self.put_existing(id, project, parsed, expected_revision, dry_run, operation)
            }
            None => self.put_new(id, parsed, expected_revision, dry_run, operation),
        }
    }

    fn validate_target_scope(&self, target: &str) -> Result<(), HandlerError> {
        if !target.starts_with("memory://") {
            return Ok(());
        }
        let valid_prefix = match &self.scope {
            BundleScope::Project(project) => {
                format!("memory://project/{}/", percent_encode(project))
            }
            BundleScope::Global => "memory://global/".to_string(),
            BundleScope::Unscoped => "memory://unscoped/".to_string(),
        };
        if target.starts_with(&valid_prefix) {
            Ok(())
        } else {
            Err(HandlerError::ScopeMismatch {
                id: target.to_string(),
                bundle: self.scope.uri(),
            })
        }
    }

    fn validate_document_scope(&self, concept: &OkfConcept) -> Result<(), HandlerError> {
        let Some(metadata) = concept.agent_memory.as_ref() else {
            return Ok(());
        };
        if let Some(scope) = metadata.scope.as_deref() {
            if scope != self.scope.scope_name() {
                return Err(HandlerError::ScopeMismatch {
                    id: metadata.id.clone().unwrap_or_else(|| "new".to_string()),
                    bundle: self.scope.uri(),
                });
            }
        }
        if let Some(project) = metadata.project.as_deref() {
            if self.scope.project() != Some(project) {
                return Err(HandlerError::ScopeMismatch {
                    id: metadata.id.clone().unwrap_or_else(|| "new".to_string()),
                    bundle: self.scope.uri(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn load_scoped(&self, id: &str) -> Result<OkfConcept, HandlerError> {
        let project = self
            .conn
            .query_row(
                "SELECT project FROM memories WHERE id = ?1",
                params![id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
        if !self.scope.contains_project(project.as_deref()) {
            return Err(HandlerError::ScopeMismatch {
                id: id.to_string(),
                bundle: self.scope.uri(),
            });
        }
        load_concept(self.conn, id).map_err(HandlerError::from)
    }

    fn normalize_candidate(&self, id: &str, revision: i64, source: &OkfConcept) -> OkfConcept {
        let mut concept = source.clone();
        let mut metadata = concept.agent_memory.take().unwrap_or(AgentMemoryMetadata {
            id: None,
            revision: None,
            memory_type: None,
            project: None,
            scope: None,
            edges: Vec::new(),
            extensions: BTreeMap::new(),
        });
        metadata.id = Some(id.to_string());
        metadata.revision = Some(revision as u64);
        metadata.project = self.scope.project().map(str::to_string);
        metadata.scope = Some(self.scope.scope_name().to_string());
        if metadata.memory_type.as_deref().is_none_or(str::is_empty) {
            metadata.memory_type = Some("user".to_string());
        }
        concept.agent_memory = Some(metadata);
        concept
    }

    fn put_existing(
        &self,
        id: String,
        project: Option<String>,
        parsed: &ParsedDocument,
        expected_revision: Option<i64>,
        dry_run: bool,
        operation: &str,
    ) -> Result<PutResult, HandlerError> {
        if !self.scope.contains_project(project.as_deref()) {
            return Err(HandlerError::ScopeMismatch {
                id,
                bundle: self.scope.uri(),
            });
        }
        let before = load_concept(self.conn, &id)?;
        let current_revision = concepts::current_revision(self.conn, &id)?;
        if let Some(expected) = expected_revision {
            if expected != current_revision {
                return Err(MemoryError::RevisionConflict {
                    id,
                    expected,
                    actual: current_revision,
                }
                .into());
            }
        }
        let candidate = self.normalize_candidate(&id, current_revision, &parsed.concept);
        let diff = diff_concepts(&before, &candidate)?;
        if dry_run || diff.is_empty() {
            return Ok(PutResult {
                id,
                revision: current_revision,
                created: false,
                changed: !diff.is_empty(),
                dry_run,
                diff,
            });
        }

        let actor = candidate
            .generated
            .as_ref()
            .map(|generated| generated.by.as_str());
        let outcome = concepts::mutate(
            self.conn,
            &id,
            operation,
            actor,
            expected_revision,
            false,
            |revision| apply_projection(self.conn, &id, &candidate, revision),
        )?;
        Ok(PutResult {
            id,
            revision: outcome.revision,
            created: false,
            changed: outcome.changed,
            dry_run: false,
            diff,
        })
    }

    fn put_new(
        &self,
        id: String,
        parsed: &ParsedDocument,
        expected_revision: Option<i64>,
        dry_run: bool,
        operation: &str,
    ) -> Result<PutResult, HandlerError> {
        if expected_revision.is_some() {
            return Err(HandlerError::InvalidTarget(
                "expected_revision cannot be used when creating a concept".to_string(),
            ));
        }
        let tombstoned: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_concept_tombstones WHERE memory_id = ?1)",
            params![id],
            |row| row.get(0),
        )?;
        if tombstoned {
            return Err(HandlerError::InvalidTarget(format!(
                "memory ID `{id}` was deleted and cannot be reused"
            )));
        }
        let candidate = self.normalize_candidate(&id, 1, &parsed.concept);
        let memory_type = candidate
            .agent_memory
            .as_ref()
            .and_then(|metadata| metadata.memory_type.clone())
            .unwrap_or_else(|| "user".to_string());
        let mut memory = Memory::new(
            candidate.body.clone(),
            (!candidate.tags.is_empty()).then(|| candidate.tags.clone()),
            self.scope.project().map(str::to_string),
            candidate
                .generated
                .as_ref()
                .map(|generated| generated.by.clone()),
            None,
            Some(memory_type),
        );
        memory.id = id.clone();
        let empty = OkfConcept::minimal("", "");
        let diff = diff_concepts(&empty, &candidate)?;
        if dry_run {
            return Ok(PutResult {
                id,
                revision: 1,
                created: true,
                changed: true,
                dry_run: true,
                diff,
            });
        }
        let actor = candidate
            .generated
            .as_ref()
            .map(|generated| generated.by.as_str());
        concepts::insert_memory_with(self.conn, &memory, operation, actor, None, |revision| {
            apply_projection(self.conn, &id, &candidate, revision)
        })?;
        Ok(PutResult {
            id,
            revision: 1,
            created: true,
            changed: true,
            dry_run: false,
            diff,
        })
    }
}

fn ensure_writable_target(target: &str) -> Result<(), HandlerError> {
    if target == "/index.md"
        || target == "index.md"
        || target == "/log.md"
        || target == "log.md"
        || target.starts_with("/types/")
        || target.starts_with("/tags/")
    {
        return Err(HandlerError::ReadOnly(target.to_string()));
    }
    if target.starts_with('/') && !target.starts_with("/memories/") {
        return Err(HandlerError::InvalidTarget(target.to_string()));
    }
    Ok(())
}

fn target_id(target: &str) -> Result<String, HandlerError> {
    let target = target.trim();
    if target.is_empty() {
        return Err(HandlerError::InvalidTarget(target.to_string()));
    }
    if let Some(name) = target.strip_prefix("/memories/") {
        return name
            .strip_suffix(".md")
            .filter(|id| !id.is_empty() && !id.contains('/'))
            .map(str::to_string)
            .ok_or_else(|| HandlerError::InvalidTarget(target.to_string()));
    }
    if target.starts_with("memory://") {
        return target
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| HandlerError::InvalidTarget(target.to_string()));
    }
    if target.contains('/') || target.ends_with(".md") {
        return Err(HandlerError::InvalidTarget(target.to_string()));
    }
    Ok(target.to_string())
}

fn status_name(status: &OkfStatus) -> &'static str {
    match status {
        OkfStatus::Draft => "draft",
        OkfStatus::Stable => "stable",
        OkfStatus::Deprecated => "deprecated",
    }
}

fn apply_projection(
    conn: &Connection,
    id: &str,
    concept: &OkfConcept,
    revision: i64,
) -> Result<(), MemoryError> {
    let metadata = concept
        .agent_memory
        .as_ref()
        .ok_or_else(|| MemoryError::Config("missing x-agent-memory metadata".to_string()))?;
    let memory_type = metadata.memory_type.as_deref().unwrap_or("user");
    let tags_json = (!concept.tags.is_empty())
        .then(|| serde_json::to_string(&concept.tags))
        .transpose()?;
    conn.execute(
        "UPDATE memories SET
                             content_raw = CASE
                                 WHEN content <> ?1 THEN COALESCE(content_raw, content)
                                 ELSE content_raw
                             END,
                             content = ?1, tags = ?2, memory_type = ?3,
                             updated_at = ?4, superseded_by = NULL
         WHERE id = ?5",
        params![
            concept.body,
            tags_json,
            memory_type,
            chrono::Utc::now().to_rfc3339(),
            id
        ],
    )?;
    let extras = StoredConceptExtras {
        extensions: concept.extensions.clone(),
        usage_window: concept.usage_window.clone(),
        runtime: concept.runtime.clone(),
        parameters: concept.parameters.clone(),
        computation: concept.computation.clone(),
        executor: concept.executor.clone(),
        attester: concept.attester.clone(),
        generated_extensions: concept
            .generated
            .as_ref()
            .map(|value| value.extensions.clone())
            .unwrap_or_default(),
        agent_extensions: metadata.extensions.clone(),
    };
    let extras_json = serde_json::to_string(&extras)?;
    conn.execute(
        "UPDATE memory_concepts SET
             concept_type = ?1, title = ?2, description = ?3, resource = ?4,
             status = ?5, stale_after = ?6, generated_by = ?7, generated_at = ?8,
             extensions_json = ?9, raw_frontmatter = ?10
         WHERE memory_id = ?11",
        params![
            concept.concept_type,
            concept.title,
            concept.description,
            concept.resource,
            status_name(&concept.status),
            concept.stale_after,
            concept.generated.as_ref().map(|value| value.by.as_str()),
            concept
                .generated
                .as_ref()
                .and_then(|value| value.at.as_deref()),
            extras_json,
            concept.raw_frontmatter,
            id,
        ],
    )?;

    conn.execute(
        "DELETE FROM memory_sources WHERE memory_id = ?1",
        params![id],
    )?;
    let mut source_keys = HashSet::new();
    for (ordinal, source) in concept.sources.iter().enumerate() {
        let key = source
            .id
            .clone()
            .unwrap_or_else(|| format!("source-{ordinal}"));
        if !source_keys.insert(key.clone()) {
            return Err(MemoryError::Config(format!(
                "duplicate OKF source id `{key}`"
            )));
        }
        conn.execute(
            "INSERT INTO memory_sources (
                 memory_id, source_key, ordinal, resource, title, author, usage_count,
                 usage_window_from, usage_window_to, last_modified, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                key,
                ordinal as i64,
                source.resource,
                source.title,
                source.author,
                source.usage_count.map(|value| value as i64),
                source
                    .usage_window
                    .as_ref()
                    .map(|value| value.from.as_str()),
                source.usage_window.as_ref().map(|value| value.to.as_str()),
                source.last_modified,
                serde_json::to_string(&source.extensions)?,
            ],
        )?;
    }

    conn.execute(
        "DELETE FROM memory_verifications WHERE memory_id = ?1",
        params![id],
    )?;
    for verification in &concept.verified {
        conn.execute(
            "INSERT INTO memory_verifications (
                 id, memory_id, actor, verified_at, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                uuid::Uuid::new_v4().to_string(),
                id,
                verification.by,
                verification.at,
                serde_json::to_string(&verification.extensions)?,
            ],
        )?;
    }

    conn.execute(
        "DELETE FROM memory_relationships
         WHERE src_memory_id = ?1
           AND producer IN ('okf-document', 'okf-markdown', 'okf-sources')",
        params![id],
    )?;
    for (ordinal, edge) in metadata.edges.iter().enumerate() {
        let producer = edge
            .extensions
            .get("x-agent-memory-producer")
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or("okf-document");
        if producer == "okf-document" {
            insert_projected_relationship(
                conn,
                id,
                &edge.target,
                &edge.relation,
                edge.confidence.as_deref().unwrap_or("asserted"),
                producer,
                revision,
                ordinal as i64,
                serde_json::to_string(&edge.extensions)?,
            )?;
        }
    }
    for link in super::extract_markdown_links(&concept.body) {
        insert_projected_relationship(
            conn,
            id,
            &link.target,
            "links_to",
            "extracted",
            "okf-markdown",
            revision,
            link.ordinal as i64,
            serde_json::to_string(&serde_json::json!({ "label": link.label }))?,
        )?;
    }
    for (ordinal, source) in concept.sources.iter().enumerate() {
        insert_projected_relationship(
            conn,
            id,
            &source.resource,
            "cites",
            "asserted",
            "okf-sources",
            revision,
            ordinal as i64,
            "{}".to_string(),
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_projected_relationship(
    conn: &Connection,
    id: &str,
    target: &str,
    relation: &str,
    confidence: &str,
    producer: &str,
    revision: i64,
    ordinal: i64,
    metadata_json: String,
) -> Result<(), MemoryError> {
    let resolved = super::resolve_memory_reference(conn, target)?;
    conn.execute(
        "INSERT INTO memory_relationships (
             id, src_memory_id, dst_memory_id, dst_ref, relation, confidence,
             producer, source_revision, ordinal, metadata_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            uuid::Uuid::new_v4().to_string(),
            id,
            resolved,
            target,
            relation,
            confidence,
            producer,
            revision,
            ordinal,
            metadata_json,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub(crate) fn load_concept(conn: &Connection, id: &str) -> Result<OkfConcept, MemoryError> {
    let memory = queries::get_memory_by_id(conn, id)?;
    let row = conn.query_row(
        "SELECT concept_type, title, description, resource, status, stale_after,
                generated_by, generated_at, extensions_json, raw_frontmatter, current_revision
         FROM memory_concepts WHERE memory_id = ?1",
        params![id],
        |row| {
            Ok(StoredConceptRow {
                concept_type: row.get(0)?,
                title: row.get(1)?,
                description: row.get(2)?,
                resource: row.get(3)?,
                status: row.get(4)?,
                stale_after: row.get(5)?,
                generated_by: row.get(6)?,
                generated_at: row.get(7)?,
                extras_json: row.get(8)?,
                raw_frontmatter: row.get(9)?,
                revision: row.get(10)?,
            })
        },
    )?;
    let extras: StoredConceptExtras = serde_json::from_str(&row.extras_json).unwrap_or_else(|_| {
        let extensions = serde_json::from_str(&row.extras_json).unwrap_or_default();
        StoredConceptExtras {
            extensions,
            ..StoredConceptExtras::default()
        }
    });
    let sources = load_sources(conn, id)?;
    let verified = load_verifications(conn, id)?;
    let edges = load_edges(conn, id)?;
    let generated = row.generated_by.map(|by| OkfGenerated {
        by,
        at: row.generated_at,
        extensions: extras.generated_extensions.clone(),
    });
    let status = match row.status.as_str() {
        "draft" => OkfStatus::Draft,
        "deprecated" => OkfStatus::Deprecated,
        _ => OkfStatus::Stable,
    };
    Ok(OkfConcept {
        concept_type: row.concept_type,
        title: row.title,
        description: row.description,
        resource: row.resource,
        tags: memory.tags.unwrap_or_default(),
        sources,
        usage_window: extras.usage_window,
        generated,
        verified,
        status,
        stale_after: row.stale_after,
        runtime: extras.runtime,
        parameters: extras.parameters,
        computation: extras.computation,
        executor: extras.executor,
        attester: extras.attester,
        agent_memory: Some(AgentMemoryMetadata {
            id: Some(id.to_string()),
            revision: Some(row.revision as u64),
            memory_type: Some(memory.memory_type.unwrap_or_else(|| "user".to_string())),
            project: memory.project.clone(),
            scope: Some(
                match memory.project.as_deref() {
                    Some(queries::GLOBAL_PROJECT_IDENT) => "global",
                    Some(_) => "project",
                    None => "unscoped",
                }
                .to_string(),
            ),
            edges,
            extensions: extras.agent_extensions,
        }),
        extensions: extras.extensions,
        body: memory.content,
        raw_frontmatter: row.raw_frontmatter,
        source_semantic_hash: None,
    })
}

fn load_sources(conn: &Connection, id: &str) -> Result<Vec<OkfSource>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT source_key, resource, title, author, usage_count, usage_window_from,
                usage_window_to, last_modified, metadata_json
         FROM memory_sources WHERE memory_id = ?1 ORDER BY ordinal, source_key",
    )?;
    let rows = stmt
        .query_map(params![id], |row| {
            let from: Option<String> = row.get(5)?;
            let to: Option<String> = row.get(6)?;
            let metadata: String = row.get(8)?;
            Ok(OkfSource {
                id: row.get(0)?,
                resource: row.get(1)?,
                title: row.get(2)?,
                author: row.get(3)?,
                usage_count: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
                usage_window: from.zip(to).map(|(from, to)| UsageWindow { from, to }),
                last_modified: row.get(7)?,
                extensions: serde_json::from_str(&metadata).unwrap_or_default(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn load_verifications(conn: &Connection, id: &str) -> Result<Vec<OkfVerification>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT actor, verified_at, metadata_json FROM memory_verifications
         WHERE memory_id = ?1 ORDER BY verified_at, id",
    )?;
    let rows = stmt
        .query_map(params![id], |row| {
            let metadata: String = row.get(2)?;
            Ok(OkfVerification {
                by: row.get(0)?,
                at: row.get(1)?,
                extensions: serde_json::from_str(&metadata).unwrap_or_default(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn load_edges(conn: &Connection, id: &str) -> Result<Vec<OkfRelationship>, MemoryError> {
    let mut stmt = conn.prepare(
        "SELECT dst_ref, relation, confidence, metadata_json, producer FROM memory_relationships
         WHERE src_memory_id = ?1 ORDER BY relation, dst_ref, producer, COALESCE(ordinal, -1), id",
    )?;
    let rows = stmt
        .query_map(params![id], |row| {
            let metadata: String = row.get(3)?;
            let producer: String = row.get(4)?;
            let mut extensions: Extensions = serde_json::from_str(&metadata).unwrap_or_default();
            extensions.insert(
                "x-agent-memory-producer".to_string(),
                serde_yaml_ng::Value::String(producer),
            );
            Ok(OkfRelationship {
                target: row.get(0)?,
                relation: row.get(1)?,
                confidence: row.get(2)?,
                extensions,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{open_database, queries};

    const DOCUMENT: &str = r#"---
type: Project Decision
title: Conflict policy
tags: [gateway, synchronization, gateway]
status: stable
generated:
  by: process:test
  at: 2026-08-15T14:00:00Z
  x-model: fixture
sources:
  - id: policy
    resource: https://example.invalid/policy
verified:
  by: human:reviewer
  at: 2026-08-15T15:00:00Z
x-extra:
  nested: value
x-agent-memory:
  memory_type: project
  edges:
    - target: external:policy
      relation: cites
---
The gateway preserves local content.
"#;

    #[test]
    fn create_render_parse_and_dry_run_are_lossless_and_noop() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let handler = OkfDocumentHandler::new(&conn, BundleScope::Project("alpha".to_string()));
        let parsed = handler.parse(DOCUMENT).expect("parse");
        let created = handler
            .put(None, &parsed, None, false)
            .expect("create concept");
        assert!(created.created);
        assert_eq!(created.revision, 1);

        let rendered = handler.render(&created.id).expect("render");
        assert!(rendered.text.contains("type: Project Decision"));
        assert!(rendered.text.contains("x-extra:"));
        assert!(rendered.text.contains("x-model: fixture"));
        assert!(rendered.text.contains("revision: 1"));
        let reparsed = handler.parse(&rendered.text).expect("reparse");
        let dry_run = handler
            .put(Some(&rendered.virtual_path), &reparsed, Some(1), true)
            .expect("dry run");
        assert!(!dry_run.changed);
        assert!(dry_run.diff.is_empty());
        assert_eq!(concepts::current_revision(&conn, &created.id).unwrap(), 1);
    }

    #[test]
    fn update_enforces_cas_target_identity_read_only_paths_and_scope() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let handler = OkfDocumentHandler::new(&conn, BundleScope::Project("alpha".to_string()));
        let parsed = handler.parse(DOCUMENT).expect("parse");
        let created = handler.put(None, &parsed, None, false).expect("create");
        let rendered = handler.render(&created.id).expect("render");
        let mut changed = handler.parse(&rendered.text).expect("parse rendered");
        changed.concept.body = "Updated policy.\n".to_string();

        let stale = handler
            .put(Some(&created.id), &changed, Some(9), false)
            .expect_err("stale CAS");
        assert!(matches!(
            stale,
            HandlerError::Memory(MemoryError::RevisionConflict { actual: 1, .. })
        ));
        let updated = handler
            .put(Some(&created.id), &changed, Some(1), false)
            .expect("update");
        assert_eq!(updated.revision, 2);

        changed.concept.agent_memory.as_mut().unwrap().id = Some("different".to_string());
        assert!(matches!(
            handler.put(Some(&created.id), &changed, None, true),
            Err(HandlerError::IdMismatch { .. })
        ));
        assert!(matches!(
            handler.put(Some("/index.md"), &parsed, None, true),
            Err(HandlerError::ReadOnly(_))
        ));
        let other = OkfDocumentHandler::new(&conn, BundleScope::Project("beta".to_string()));
        assert!(matches!(
            other.render(&created.id),
            Err(HandlerError::ScopeMismatch { .. })
        ));
    }

    #[test]
    fn working_context_is_not_addressable_as_a_document() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        queries::set_working_context(&conn, "alpha", "transient").expect("working context");
        let handler = OkfDocumentHandler::new(&conn, BundleScope::Project("alpha".to_string()));
        assert!(matches!(
            handler.render("alpha"),
            Err(HandlerError::Memory(MemoryError::NotFound(_)))
        ));
    }

    #[test]
    fn reparse_replaces_only_document_owned_extracted_relationships() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let mut target = Memory::new(
            "target".to_string(),
            None,
            Some("alpha".to_string()),
            None,
            None,
            None,
        );
        target.id = "target-id".to_string();
        queries::insert_memory(&conn, &target).expect("target");
        let handler = OkfDocumentHandler::new(&conn, BundleScope::Project("alpha".to_string()));
        let document = r#"---
type: Reference
sources:
  - resource: https://example.invalid/source
x-agent-memory:
  edges:
    - target: external:explicit
      relation: applies_to
---
See [target](/memories/target-id.md) and [broken](/memories/missing.md).
"#;
        let parsed = handler.parse(document).expect("parse");
        let created = handler.put(None, &parsed, None, false).expect("put");
        concepts::mutate(
            &conn,
            &created.id,
            "curate",
            None,
            Some(1),
            false,
            |revision| {
                concepts::insert_relationship(
                    &conn,
                    &created.id,
                    None,
                    "external:curated",
                    "contradicts",
                    revision,
                    None,
                )
            },
        )
        .expect("curated edge");

        let producers: Vec<(String, i64)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT producer, COUNT(*) FROM memory_relationships
                     WHERE src_memory_id = ?1 GROUP BY producer ORDER BY producer",
                )
                .expect("prepare");
            stmt.query_map(params![created.id], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("query")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect")
        };
        assert_eq!(
            producers,
            vec![
                ("agent-memory".to_string(), 1),
                ("okf-document".to_string(), 1),
                ("okf-markdown".to_string(), 2),
                ("okf-sources".to_string(), 1),
            ]
        );
        assert_eq!(
            conn.query_row(
                "SELECT dst_memory_id FROM memory_relationships
                 WHERE src_memory_id = ?1 AND dst_ref = '/memories/target-id.md'",
                params![created.id],
                |row| row.get::<_, Option<String>>(0),
            )
            .expect("resolved"),
            Some("target-id".to_string())
        );

        let rendered = handler.render(&created.id).expect("render");
        let mut changed = handler.parse(&rendered.text).expect("parse rendered");
        changed.concept.body = "No links remain.\n".to_string();
        changed.concept.sources.clear();
        handler
            .put(Some(&created.id), &changed, Some(2), false)
            .expect("update");
        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT producer FROM memory_relationships
                     WHERE src_memory_id = ?1 ORDER BY producer",
                )
                .expect("prepare");
            stmt.query_map(params![created.id], |row| row.get(0))
                .expect("query")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect")
        };
        assert_eq!(remaining, vec!["agent-memory", "okf-document"]);
    }
}
