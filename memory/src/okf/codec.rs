use std::collections::BTreeSet;

use pulldown_cmark::{Event, Parser, Tag};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::json;
use serde_yaml_ng::{Mapping, Value as YamlValue};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{
    AgentMemoryMetadata, Extensions, OkfAttester, OkfConcept, OkfExecutor, OkfGenerated,
    OkfParameter, OkfSource, OkfStatus, OkfVerification, UsageWindow,
};

const MAX_DOCUMENT_BYTES: usize = 1_048_576;
const MAX_FRONTMATTER_BYTES: usize = 131_072;
const MAX_YAML_DEPTH: usize = 32;
const MAX_YAML_ITEMS: usize = 10_000;
const MAX_YAML_ALIASES: usize = 64;
const MAX_TAGS: usize = 256;
const MAX_SOURCES: usize = 256;
const MAX_VERIFICATIONS: usize = 256;
const MAX_PARAMETERS: usize = 256;
const MAX_LINKS: usize = 2_048;

const RESERVED_KEYS: &[&str] = &[
    "type",
    "title",
    "description",
    "resource",
    "tags",
    "sources",
    "usage_window",
    "generated",
    "verified",
    "status",
    "stale_after",
    "runtime",
    "parameters",
    "computation",
    "executor",
    "attester",
    "x-agent-memory",
];

#[derive(Debug, Error)]
pub enum OkfError {
    #[error("OKF document exceeds {limit} bytes (actual {actual})")]
    DocumentTooLarge { limit: usize, actual: usize },
    #[error("OKF frontmatter exceeds {limit} bytes (actual {actual})")]
    FrontmatterTooLarge { limit: usize, actual: usize },
    #[error("OKF document must start with YAML frontmatter delimited by `---`")]
    MissingFrontmatter,
    #[error("OKF frontmatter is not terminated by `---`")]
    UnterminatedFrontmatter,
    #[error("OKF frontmatter must be a mapping")]
    FrontmatterNotMapping,
    #[error("OKF frontmatter key must be a string")]
    NonStringKey,
    #[error("OKF field `{field}` has an invalid shape: {message}")]
    InvalidField { field: String, message: String },
    #[error("OKF field `type` is required and must be non-empty")]
    MissingType,
    #[error("OKF input exceeds structural limit `{name}` ({actual} > {limit})")]
    LimitExceeded {
        name: &'static str,
        limit: usize,
        actual: usize,
    },
    #[error("OKF YAML parse failed: {0}")]
    Yaml(String),
    #[error("OKF serialization failed: {0}")]
    Serialization(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OkfDiagnostic {
    pub code: &'static str,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDocument {
    pub concept: OkfConcept,
    pub diagnostics: Vec<OkfDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Normalized,
    PreserveRawWhenUnchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDiff {
    pub field: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDiff {
    pub fields: Vec<FieldDiff>,
}

impl SemanticDiff {
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

pub fn parse_document(input: &str) -> Result<ParsedDocument, OkfError> {
    if input.len() > MAX_DOCUMENT_BYTES {
        return Err(OkfError::DocumentTooLarge {
            limit: MAX_DOCUMENT_BYTES,
            actual: input.len(),
        });
    }

    let (raw_frontmatter, body) = split_document(input)?;
    if raw_frontmatter.len() > MAX_FRONTMATTER_BYTES {
        return Err(OkfError::FrontmatterTooLarge {
            limit: MAX_FRONTMATTER_BYTES,
            actual: raw_frontmatter.len(),
        });
    }
    enforce_alias_limit(raw_frontmatter)?;

    let yaml: YamlValue = serde_yaml_ng::from_str(raw_frontmatter)
        .map_err(|error| OkfError::Yaml(error.to_string()))?;
    let mapping = yaml.as_mapping().ok_or(OkfError::FrontmatterNotMapping)?;
    enforce_yaml_limits(&yaml)?;

    let mut values = string_keyed_mapping(mapping)?;
    let mut diagnostics = Vec::new();

    let concept_type = take_required_string(&mut values, "type")?;
    let title = take_optional_string(&mut values, "title")?;
    let description = take_optional_string(&mut values, "description")?;
    let resource = take_optional_string(&mut values, "resource")?;
    let tags =
        normalize_tags(take_optional::<Vec<String>>(&mut values, "tags")?.unwrap_or_default())?;
    let sources = take_optional::<Vec<OkfSource>>(&mut values, "sources")?.unwrap_or_default();
    if sources.len() > MAX_SOURCES {
        return Err(limit("sources", MAX_SOURCES, sources.len()));
    }
    validate_sources(&sources)?;
    let usage_window = take_optional::<UsageWindow>(&mut values, "usage_window")?;
    let generated = take_optional::<OkfGenerated>(&mut values, "generated")?;
    if let Some(item) = &generated {
        if item.by.trim().is_empty() {
            return Err(invalid("generated", "`by` must be non-empty"));
        }
    }
    let verified = take_verified(&mut values)?;
    let status = take_optional::<OkfStatus>(&mut values, "status")?.unwrap_or_default();
    let stale_after = take_optional_string(&mut values, "stale_after")?;
    let runtime = take_optional_string(&mut values, "runtime")?;
    let parameters =
        take_optional::<Vec<OkfParameter>>(&mut values, "parameters")?.unwrap_or_default();
    if parameters.len() > MAX_PARAMETERS {
        return Err(limit("parameters", MAX_PARAMETERS, parameters.len()));
    }
    let computation = take_optional_string(&mut values, "computation")?;
    let executor = take_optional::<OkfExecutor>(&mut values, "executor")?;
    let attester = take_optional::<OkfAttester>(&mut values, "attester")?;
    let agent_memory = take_optional::<AgentMemoryMetadata>(&mut values, "x-agent-memory")?;

    if concept_type == "Attested Computation" && runtime.as_deref().is_none_or(str::is_empty) {
        return Err(invalid(
            "runtime",
            "is required for `type: Attested Computation`",
        ));
    }

    if values.contains_key("timestamp") && generated.is_none() {
        diagnostics.push(warning(
            "okf.legacy_timestamp",
            "legacy `timestamp` retained as an extension; prefer `generated.at`",
        ));
    }
    if sources.is_empty() && has_citations_heading(body) {
        diagnostics.push(warning(
            "okf.legacy_citations",
            "legacy `# Citations` body section accepted; prefer frontmatter `sources`",
        ));
    }

    validate_extension_keys(&values)?;
    let link_count = count_links(body);
    if link_count > MAX_LINKS {
        return Err(limit("markdown_links", MAX_LINKS, link_count));
    }

    let mut concept = OkfConcept {
        concept_type,
        title,
        description,
        resource,
        tags,
        sources,
        usage_window,
        generated,
        verified,
        status,
        stale_after,
        runtime,
        parameters,
        computation,
        executor,
        attester,
        agent_memory,
        extensions: values,
        body: body.to_string(),
        raw_frontmatter: Some(raw_frontmatter.to_string()),
        source_semantic_hash: None,
    };
    concept.source_semantic_hash = Some(semantic_hash(&concept)?);

    Ok(ParsedDocument {
        concept,
        diagnostics,
    })
}

pub fn render_document(concept: &OkfConcept, mode: RenderMode) -> Result<String, OkfError> {
    validate_concept(concept)?;
    if mode == RenderMode::PreserveRawWhenUnchanged {
        if let (Some(raw), Some(source_hash)) = (
            concept.raw_frontmatter.as_deref(),
            concept.source_semantic_hash.as_deref(),
        ) {
            if semantic_hash(concept)? == source_hash {
                return Ok(format!("---\n{raw}\n---\n{}", concept.body));
            }
        }
    }

    let mut mapping = Mapping::new();
    insert_value(&mut mapping, "type", &concept.concept_type)?;
    insert_optional(&mut mapping, "title", concept.title.as_ref())?;
    insert_optional(&mut mapping, "description", concept.description.as_ref())?;
    insert_optional(&mut mapping, "resource", concept.resource.as_ref())?;
    if !concept.tags.is_empty() {
        insert_value(&mut mapping, "tags", &concept.tags)?;
    }
    if !concept.sources.is_empty() {
        insert_value(&mut mapping, "sources", &concept.sources)?;
    }
    insert_optional(&mut mapping, "usage_window", concept.usage_window.as_ref())?;
    insert_optional(&mut mapping, "generated", concept.generated.as_ref())?;
    if !concept.verified.is_empty() {
        insert_value(&mut mapping, "verified", &concept.verified)?;
    }
    insert_value(&mut mapping, "status", &concept.status)?;
    insert_optional(&mut mapping, "stale_after", concept.stale_after.as_ref())?;
    insert_optional(&mut mapping, "runtime", concept.runtime.as_ref())?;
    if !concept.parameters.is_empty() {
        insert_value(&mut mapping, "parameters", &concept.parameters)?;
    }
    insert_optional(&mut mapping, "computation", concept.computation.as_ref())?;
    insert_optional(&mut mapping, "executor", concept.executor.as_ref())?;
    insert_optional(&mut mapping, "attester", concept.attester.as_ref())?;
    insert_optional(
        &mut mapping,
        "x-agent-memory",
        concept.agent_memory.as_ref(),
    )?;

    for (key, value) in &concept.extensions {
        if RESERVED_KEYS.contains(&key.as_str()) {
            return Err(invalid(
                key,
                "extension attempts to shadow a reserved normalized field",
            ));
        }
        mapping.insert(YamlValue::String(key.clone()), value.clone());
    }

    let yaml = serde_yaml_ng::to_string(&mapping)
        .map_err(|error| OkfError::Serialization(error.to_string()))?;
    Ok(format!("---\n{}---\n{}", yaml, concept.body))
}

pub fn semantic_hash(concept: &OkfConcept) -> Result<String, OkfError> {
    validate_concept(concept)?;
    let value = semantic_value(concept)?;
    let bytes =
        serde_json::to_vec(&value).map_err(|error| OkfError::Serialization(error.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn diff_concepts(before: &OkfConcept, after: &OkfConcept) -> Result<SemanticDiff, OkfError> {
    let before = semantic_value(before)?;
    let after = semantic_value(after)?;
    let before = before
        .as_object()
        .ok_or_else(|| OkfError::Serialization("semantic snapshot is not an object".into()))?;
    let after = after
        .as_object()
        .ok_or_else(|| OkfError::Serialization("semantic snapshot is not an object".into()))?;
    let keys: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    let mut fields = Vec::new();
    for key in keys {
        if before.get(&key) != after.get(&key) {
            fields.push(FieldDiff {
                field: key.clone(),
                before: before.get(&key).map(ToString::to_string),
                after: after.get(&key).map(ToString::to_string),
            });
        }
    }
    Ok(SemanticDiff { fields })
}

fn semantic_value(concept: &OkfConcept) -> Result<serde_json::Value, OkfError> {
    let extensions = yaml_extensions_to_json(&concept.extensions)?;
    Ok(json!({
        "type": concept.concept_type,
        "title": concept.title,
        "description": concept.description,
        "resource": concept.resource,
        "tags": concept.tags,
        "sources": concept.sources,
        "usage_window": concept.usage_window,
        "generated": concept.generated,
        "verified": concept.verified,
        "status": concept.status,
        "stale_after": concept.stale_after,
        "runtime": concept.runtime,
        "parameters": concept.parameters,
        "computation": concept.computation,
        "executor": concept.executor,
        "attester": concept.attester,
        "x-agent-memory": concept.agent_memory,
        "extensions": extensions,
        "body": concept.body,
    }))
}

fn yaml_extensions_to_json(extensions: &Extensions) -> Result<serde_json::Value, OkfError> {
    let yaml = serde_yaml_ng::to_value(extensions)
        .map_err(|error| OkfError::Serialization(error.to_string()))?;
    serde_json::to_value(yaml).map_err(|error| OkfError::Serialization(error.to_string()))
}

fn validate_concept(concept: &OkfConcept) -> Result<(), OkfError> {
    if concept.concept_type.trim().is_empty() {
        return Err(OkfError::MissingType);
    }
    if concept.tags.len() > MAX_TAGS {
        return Err(limit("tags", MAX_TAGS, concept.tags.len()));
    }
    if concept.sources.len() > MAX_SOURCES {
        return Err(limit("sources", MAX_SOURCES, concept.sources.len()));
    }
    if concept.verified.len() > MAX_VERIFICATIONS {
        return Err(limit("verified", MAX_VERIFICATIONS, concept.verified.len()));
    }
    if concept.parameters.len() > MAX_PARAMETERS {
        return Err(limit(
            "parameters",
            MAX_PARAMETERS,
            concept.parameters.len(),
        ));
    }
    validate_sources(&concept.sources)?;
    if concept
        .generated
        .as_ref()
        .is_some_and(|item| item.by.trim().is_empty())
    {
        return Err(invalid("generated", "`by` must be non-empty"));
    }
    if concept
        .verified
        .iter()
        .any(|item| item.by.trim().is_empty() || item.at.trim().is_empty())
    {
        return Err(invalid("verified", "`by` and `at` must be non-empty"));
    }
    let link_count = count_links(&concept.body);
    if link_count > MAX_LINKS {
        return Err(limit("markdown_links", MAX_LINKS, link_count));
    }
    validate_extension_keys(&concept.extensions)?;
    let yaml = serde_yaml_ng::to_value(&concept.extensions)
        .map_err(|error| OkfError::Serialization(error.to_string()))?;
    enforce_yaml_limits(&yaml)
}

fn validate_sources(sources: &[OkfSource]) -> Result<(), OkfError> {
    let mut ids = BTreeSet::new();
    for source in sources {
        if source.resource.trim().is_empty() {
            return Err(invalid("sources", "every source requires `resource`"));
        }
        if let Some(id) = source.id.as_deref() {
            if id.trim().is_empty() || !ids.insert(id) {
                return Err(invalid(
                    "sources",
                    "source IDs must be non-empty and unique",
                ));
            }
        }
    }
    Ok(())
}

fn split_document(input: &str) -> Result<(&str, &str), OkfError> {
    let normalized = input.strip_prefix('\u{feff}').unwrap_or(input);
    let after_open = normalized
        .strip_prefix("---\n")
        .or_else(|| normalized.strip_prefix("---\r\n"))
        .ok_or(OkfError::MissingFrontmatter)?;

    let mut offset = 0;
    for segment in after_open.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\r', '\n']);
        if line == "---" {
            let raw = &after_open[..offset];
            let body_start = offset + segment.len();
            return Ok((
                raw.trim_end_matches(['\r', '\n']),
                &after_open[body_start..],
            ));
        }
        offset += segment.len();
    }
    Err(OkfError::UnterminatedFrontmatter)
}

fn string_keyed_mapping(mapping: &Mapping) -> Result<Extensions, OkfError> {
    mapping
        .iter()
        .map(|(key, value)| {
            let key = key.as_str().ok_or(OkfError::NonStringKey)?.to_string();
            Ok((key, value.clone()))
        })
        .collect()
}

fn take_required_string(values: &mut Extensions, key: &str) -> Result<String, OkfError> {
    let value = take_optional_string(values, key)?.ok_or(OkfError::MissingType)?;
    if value.trim().is_empty() {
        return Err(OkfError::MissingType);
    }
    Ok(value)
}

fn take_optional_string(values: &mut Extensions, key: &str) -> Result<Option<String>, OkfError> {
    let Some(value) = values.remove(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|value| Some(value.to_string()))
        .ok_or_else(|| invalid(key, "expected a string"))
}

fn take_optional<T: DeserializeOwned>(
    values: &mut Extensions,
    key: &str,
) -> Result<Option<T>, OkfError> {
    let Some(value) = values.remove(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_yaml_ng::from_value(value)
        .map(Some)
        .map_err(|error| invalid(key, error.to_string()))
}

fn take_verified(values: &mut Extensions) -> Result<Vec<OkfVerification>, OkfError> {
    let Some(value) = values.remove("verified") else {
        return Ok(Vec::new());
    };
    let verified = if value.is_sequence() {
        serde_yaml_ng::from_value::<Vec<OkfVerification>>(value)
    } else if value.is_mapping() {
        serde_yaml_ng::from_value::<OkfVerification>(value).map(|item| vec![item])
    } else {
        return Err(invalid("verified", "expected a mapping or list"));
    }
    .map_err(|error| invalid("verified", error.to_string()))?;

    if verified.len() > MAX_VERIFICATIONS {
        return Err(limit("verified", MAX_VERIFICATIONS, verified.len()));
    }
    if verified
        .iter()
        .any(|item| item.by.trim().is_empty() || item.at.trim().is_empty())
    {
        return Err(invalid("verified", "`by` and `at` must be non-empty"));
    }
    Ok(verified)
}

fn normalize_tags(tags: Vec<String>) -> Result<Vec<String>, OkfError> {
    if tags.len() > MAX_TAGS {
        return Err(limit("tags", MAX_TAGS, tags.len()));
    }
    let mut seen = BTreeSet::new();
    Ok(tags
        .into_iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty() && seen.insert(tag.clone()))
        .collect())
}

fn insert_value<T: Serialize>(mapping: &mut Mapping, key: &str, value: &T) -> Result<(), OkfError> {
    let value = serde_yaml_ng::to_value(value)
        .map_err(|error| OkfError::Serialization(error.to_string()))?;
    mapping.insert(YamlValue::String(key.to_string()), value);
    Ok(())
}

fn insert_optional<T: Serialize>(
    mapping: &mut Mapping,
    key: &str,
    value: Option<&T>,
) -> Result<(), OkfError> {
    if let Some(value) = value {
        insert_value(mapping, key, value)?;
    }
    Ok(())
}

fn validate_extension_keys(extensions: &Extensions) -> Result<(), OkfError> {
    for key in extensions.keys() {
        if key.trim().is_empty() {
            return Err(invalid("extensions", "keys must be non-empty"));
        }
    }
    Ok(())
}

fn enforce_yaml_limits(value: &YamlValue) -> Result<(), OkfError> {
    fn walk(value: &YamlValue, depth: usize, items: &mut usize) -> Result<(), OkfError> {
        if depth > MAX_YAML_DEPTH {
            return Err(limit("yaml_depth", MAX_YAML_DEPTH, depth));
        }
        *items += 1;
        if *items > MAX_YAML_ITEMS {
            return Err(limit("yaml_items", MAX_YAML_ITEMS, *items));
        }
        match value {
            YamlValue::Sequence(values) => {
                for value in values {
                    walk(value, depth + 1, items)?;
                }
            }
            YamlValue::Mapping(values) => {
                for (key, value) in values {
                    if !key.is_string() {
                        return Err(OkfError::NonStringKey);
                    }
                    walk(value, depth + 1, items)?;
                }
            }
            YamlValue::Tagged(_) => {
                return Err(invalid("frontmatter", "YAML tags are not supported"));
            }
            _ => {}
        }
        Ok(())
    }
    walk(value, 0, &mut 0)
}

fn enforce_alias_limit(raw: &str) -> Result<(), OkfError> {
    let count = raw
        .lines()
        .flat_map(str::split_whitespace)
        .filter(|token| token.starts_with('*'))
        .count();
    if count > MAX_YAML_ALIASES {
        return Err(limit("yaml_aliases", MAX_YAML_ALIASES, count));
    }
    Ok(())
}

fn count_links(body: &str) -> usize {
    Parser::new(body)
        .filter(|event| matches!(event, Event::Start(Tag::Link { .. })))
        .count()
}

fn has_citations_heading(body: &str) -> bool {
    body.lines()
        .any(|line| line.trim().eq_ignore_ascii_case("# citations"))
}

fn invalid(field: impl Into<String>, message: impl Into<String>) -> OkfError {
    OkfError::InvalidField {
        field: field.into(),
        message: message.into(),
    }
}

fn limit(name: &'static str, limit: usize, actual: usize) -> OkfError {
    OkfError::LimitExceeded {
        name,
        limit,
        actual,
    }
}

fn warning(code: &'static str, message: impl Into<String>) -> OkfDiagnostic {
    OkfDiagnostic {
        code,
        severity: DiagnosticSeverity::Warning,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::okf::{OkfRelationship, TrustTier};

    const COMPLETE: &str = r#"---
type: Project Decision
title: Gateway conflict policy
description: Preserve local content on conflicts.
resource: memory://project/agent-memory/01234567
tags: [gateway, sync, gateway]
sources:
  - id: contract
    resource: https://example.test/contract
    title: Gateway contract
    author: human:owner
generated: { by: process:memory-dream, at: 2026-08-15T12:00:00Z }
verified: { by: human:owner, at: 2026-08-15T13:00:00Z }
status: stable
stale_after: 2027-01-01
x-agent-memory:
  id: 01234567-89ab-cdef-0123-456789abcdef
  revision: 3
  memory_type: project
  edges:
    - target: memory://global/other
      relation: derived_from
x-domain:
  owner: platform
---
The gateway preserves local content when a conflict is detected.
"#;

    #[test]
    fn minimal_plain_text_body_round_trips() {
        let input = "---\ntype: Agent Memory/project\n---\nplain text is valid markdown";
        let parsed = parse_document(input).unwrap();
        assert_eq!(parsed.concept.body, "plain text is valid markdown");
        assert_eq!(parsed.concept.status, OkfStatus::Stable);
        let rendered = render_document(&parsed.concept, RenderMode::Normalized).unwrap();
        let reparsed = parse_document(&rendered).unwrap();
        assert_eq!(
            semantic_hash(&parsed.concept).unwrap(),
            semantic_hash(&reparsed.concept).unwrap()
        );
    }

    #[test]
    fn complete_document_normalizes_bare_verification_and_extensions() {
        let parsed = parse_document(COMPLETE).unwrap();
        assert_eq!(parsed.concept.tags, vec!["gateway", "sync"]);
        assert_eq!(parsed.concept.verified.len(), 1);
        assert_eq!(parsed.concept.trust_tier(), TrustTier::HumanReviewed);
        assert!(parsed.concept.extensions.contains_key("x-domain"));
        assert_eq!(
            parsed.concept.agent_memory.as_ref().unwrap().edges.first(),
            Some(&OkfRelationship {
                target: "memory://global/other".into(),
                relation: "derived_from".into(),
                confidence: None,
                extensions: BTreeMap::new(),
            })
        );

        let rendered = render_document(&parsed.concept, RenderMode::Normalized).unwrap();
        assert!(rendered.contains("verified:\n- by: human:owner"));
        assert!(rendered.contains("x-domain:"));
        let reparsed = parse_document(&rendered).unwrap();
        assert_eq!(
            semantic_hash(&parsed.concept).unwrap(),
            semantic_hash(&reparsed.concept).unwrap()
        );
    }

    #[test]
    fn preservation_mode_reuses_raw_only_while_semantically_unchanged() {
        let parsed =
            parse_document("---\ntype:  Reference\ncustom: {z: 1, a: 2}\n---\nbody").unwrap();
        let preserved =
            render_document(&parsed.concept, RenderMode::PreserveRawWhenUnchanged).unwrap();
        assert!(preserved.contains("type:  Reference"));

        let mut changed = parsed.concept;
        changed.title = Some("New title".into());
        let rendered = render_document(&changed, RenderMode::PreserveRawWhenUnchanged).unwrap();
        assert!(rendered.contains("title: New title"));
    }

    #[test]
    fn legacy_fields_warn_without_rejection() {
        let parsed = parse_document(
            "---\ntype: Reference\ntimestamp: 2026-01-01T00:00:00Z\n---\n# Citations\n- old",
        )
        .unwrap();
        let codes: Vec<_> = parsed.diagnostics.iter().map(|item| item.code).collect();
        assert_eq!(codes, vec!["okf.legacy_timestamp", "okf.legacy_citations"]);
        assert!(parsed.concept.extensions.contains_key("timestamp"));
    }

    #[test]
    fn attested_computation_requires_runtime_but_never_executes() {
        let error = parse_document(
            "---\ntype: Attested Computation\n---\n# Computation\n```sql\nselect 1\n```",
        )
        .unwrap_err();
        assert!(matches!(error, OkfError::InvalidField { ref field, .. } if field == "runtime"));

        let parsed = parse_document(
            "---\ntype: Attested Computation\nruntime: postgres\nexecutor: {resource: scripts/run.sh}\nattester: {resource: scripts/check.sh}\n---\nbody",
        )
        .unwrap();
        assert_eq!(parsed.concept.runtime.as_deref(), Some("postgres"));
    }

    #[test]
    fn missing_type_and_unsafe_yaml_shapes_are_rejected() {
        assert!(matches!(
            parse_document("---\ntitle: nope\n---\nbody"),
            Err(OkfError::MissingType)
        ));
        assert!(matches!(
            parse_document("---\ntype: !custom tagged\n---\nbody"),
            Err(OkfError::InvalidField { .. })
        ));
        assert!(matches!(
            parse_document("---\n? [complex, key]\n: value\ntype: Reference\n---\nbody"),
            Err(OkfError::NonStringKey)
        ));
    }

    #[test]
    fn semantic_hash_and_diff_ignore_raw_format_but_detect_body_change() {
        let first = parse_document("---\ntype: Reference\ntags: [a, b]\n---\nbody")
            .unwrap()
            .concept;
        let same = parse_document("---\ntags:\n  - a\n  - b\ntype: Reference\n---\nbody")
            .unwrap()
            .concept;
        assert_eq!(
            semantic_hash(&first).unwrap(),
            semantic_hash(&same).unwrap()
        );

        let mut changed = same;
        changed.body = "new body".into();
        let diff = diff_concepts(&first, &changed).unwrap();
        assert_eq!(
            diff.fields
                .iter()
                .map(|item| item.field.as_str())
                .collect::<Vec<_>>(),
            vec!["body"]
        );
    }
}
