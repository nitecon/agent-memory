//! OKF-native concept model and lossless document codec.
//!
//! Durable memories are the canonical concepts. Markdown is a projection of
//! the structured metadata plus the existing text body, not a second store.

mod bundle;
mod codec;
mod handlers;
mod interchange;
mod links;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value as YamlValue;

pub use bundle::{
    BundlePage, OkfBundleHandler, VirtualEntry, VirtualEntryKind, VirtualEntrySummary,
};
pub use codec::{
    diff_concepts, parse_document, render_document, semantic_hash, FieldDiff, OkfDiagnostic,
    OkfError, ParsedDocument, RenderMode, SemanticDiff,
};
pub use handlers::{BundleScope, HandlerError, OkfDocumentHandler, PutResult, RenderedDocument};
pub use interchange::reject_secret_markers;
pub use interchange::{export_bundle, import_bundle, ExportResult, ImportResult};
pub use links::{extract_markdown_links, resolve_memory_reference, ExtractedLink, LinkDiagnostic};

/// Arbitrary producer-defined OKF metadata, ordered for deterministic output.
pub type Extensions = BTreeMap<String, YamlValue>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OkfStatus {
    Draft,
    #[default]
    Stable,
    Deprecated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageWindow {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OkfSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub resource: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_window: Option<UsageWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(flatten)]
    pub extensions: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OkfGenerated {
    pub by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(flatten)]
    pub extensions: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OkfVerification {
    pub by: String,
    pub at: String,
    #[serde(flatten)]
    pub extensions: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OkfParameter {
    pub name: String,
    #[serde(rename = "type")]
    pub parameter_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(flatten)]
    pub extensions: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OkfExecutor {
    pub resource: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receipt: Vec<String>,
    #[serde(flatten)]
    pub extensions: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OkfAttester {
    pub resource: String,
    #[serde(flatten)]
    pub extensions: Extensions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OkfRelationship {
    pub target: String,
    #[serde(default = "default_relation")]
    pub relation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    #[serde(flatten)]
    pub extensions: Extensions,
}

fn default_relation() -> String {
    "links_to".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentMemoryMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<OkfRelationship>,
    #[serde(flatten)]
    pub extensions: Extensions,
}

/// The complete semantic OKF view of one durable memory.
#[derive(Debug, Clone, PartialEq)]
pub struct OkfConcept {
    pub concept_type: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub resource: Option<String>,
    pub tags: Vec<String>,
    pub sources: Vec<OkfSource>,
    pub usage_window: Option<UsageWindow>,
    pub generated: Option<OkfGenerated>,
    pub verified: Vec<OkfVerification>,
    pub status: OkfStatus,
    pub stale_after: Option<String>,
    pub runtime: Option<String>,
    pub parameters: Vec<OkfParameter>,
    pub computation: Option<String>,
    pub executor: Option<OkfExecutor>,
    pub attester: Option<OkfAttester>,
    pub agent_memory: Option<AgentMemoryMetadata>,
    pub extensions: Extensions,
    pub body: String,
    /// Original frontmatter, retained only for preservation-mode rendering.
    pub raw_frontmatter: Option<String>,
    /// Semantic hash immediately after parsing the original frontmatter/body.
    pub source_semantic_hash: Option<String>,
}

impl OkfConcept {
    pub fn minimal(concept_type: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            concept_type: concept_type.into(),
            title: None,
            description: None,
            resource: None,
            tags: Vec::new(),
            sources: Vec::new(),
            usage_window: None,
            generated: None,
            verified: Vec::new(),
            status: OkfStatus::Stable,
            stale_after: None,
            runtime: None,
            parameters: Vec::new(),
            computation: None,
            executor: None,
            attester: None,
            agent_memory: None,
            extensions: BTreeMap::new(),
            body: body.into(),
            raw_frontmatter: None,
            source_semantic_hash: None,
        }
    }

    pub fn trust_tier(&self) -> TrustTier {
        if self
            .verified
            .iter()
            .any(|item| item.by.starts_with("human:"))
        {
            TrustTier::HumanReviewed
        } else if self.verified.is_empty() {
            TrustTier::Unverified
        } else {
            TrustTier::MachineConfirmed
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustTier {
    Unverified,
    MachineConfirmed,
    HumanReviewed,
}
