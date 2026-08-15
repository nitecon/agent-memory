use pulldown_cmark::{Event, Parser, Tag};
use rusqlite::{params, Connection, OptionalExtension};

use crate::concepts;
use crate::error::MemoryError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLink {
    pub target: String,
    pub label: String,
    pub ordinal: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDiagnostic {
    pub target: String,
    pub message: String,
}

pub fn extract_markdown_links(body: &str) -> Vec<ExtractedLink> {
    let mut links = Vec::new();
    let mut active: Option<(String, String)> = None;
    for event in Parser::new(body) {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                active = Some((dest_url.into_string(), String::new()));
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, label)) = active.as_mut() {
                    label.push_str(&text);
                }
            }
            Event::End(pulldown_cmark::TagEnd::Link) => {
                if let Some((target, label)) = active.take() {
                    links.push(ExtractedLink {
                        target,
                        label,
                        ordinal: links.len(),
                    });
                }
            }
            _ => {}
        }
    }
    links
}

/// Resolve only local virtual/canonical memory references. No external input
/// causes I/O beyond the local SQLite lookup.
pub fn resolve_memory_reference(
    conn: &Connection,
    reference: &str,
) -> Result<Option<String>, MemoryError> {
    let reference = reference
        .split('#')
        .next()
        .unwrap_or(reference)
        .split('?')
        .next()
        .unwrap_or(reference);
    let candidate = if let Some(path) = reference.strip_prefix("/memories/") {
        path.strip_suffix(".md")
    } else if let Some(path) = reference.strip_prefix("../memories/") {
        path.strip_suffix(".md")
    } else if reference.starts_with("memory://") {
        reference.trim_end_matches('/').rsplit('/').next()
    } else if !reference.contains('/') {
        reference.strip_suffix(".md")
    } else {
        None
    };
    let Some(candidate) = candidate.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let project = conn
        .query_row(
            "SELECT project FROM memories WHERE id = ?1",
            params![candidate],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let Some(project) = project else {
        return Ok(None);
    };
    if reference.starts_with("memory://")
        && concepts::canonical_uri(candidate, project.as_deref()) != reference
    {
        return Ok(None);
    }
    Ok(Some(candidate.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::Memory, open_database, queries};

    #[test]
    fn extracts_links_without_fetching_or_interpreting_external_targets() {
        let links = extract_markdown_links(
            "See [one](/memories/a.md), [`two`](b.md), and [web](https://example.com).",
        );
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].target, "/memories/a.md");
        assert_eq!(links[1].label, "two");
        assert_eq!(links[2].target, "https://example.com");
    }

    #[test]
    fn resolves_exact_virtual_and_canonical_references_only() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        let mut memory = Memory::new(
            "body".to_string(),
            None,
            Some("alpha".to_string()),
            None,
            None,
            None,
        );
        memory.id = "memory-id".to_string();
        queries::insert_memory(&conn, &memory).expect("insert");
        assert_eq!(
            resolve_memory_reference(&conn, "/memories/memory-id.md").unwrap(),
            Some("memory-id".to_string())
        );
        assert_eq!(
            resolve_memory_reference(&conn, "memory://project/alpha/memory-id").unwrap(),
            Some("memory-id".to_string())
        );
        assert_eq!(
            resolve_memory_reference(&conn, "memory://project/beta/memory-id").unwrap(),
            None
        );
        assert_eq!(
            resolve_memory_reference(&conn, "https://example.com/memory-id").unwrap(),
            None
        );
    }
}
