use std::collections::BTreeMap;

use rusqlite::{params_from_iter, types::Value, Connection};

use super::{BundleScope, HandlerError, OkfDocumentHandler};
use crate::db::queries;

const PREVIEW_CHARS: usize = 160;
const DEFAULT_LOG_LIMIT: usize = 50;
const MAX_LOG_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualEntryKind {
    Document,
    Index,
    Log,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualEntry {
    pub path: String,
    pub kind: VirtualEntryKind,
    pub read_only: bool,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualEntrySummary {
    pub path: String,
    pub kind: VirtualEntryKind,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePage {
    pub document: VirtualEntry,
    pub next_cursor: Option<usize>,
}

pub struct OkfBundleHandler<'a> {
    conn: &'a Connection,
    scope: BundleScope,
}

impl<'a> OkfBundleHandler<'a> {
    pub fn new(conn: &'a Connection, scope: BundleScope) -> Self {
        Self { conn, scope }
    }

    pub fn uri(&self) -> String {
        self.scope.uri()
    }

    pub fn read(&self, path: &str) -> Result<VirtualEntry, HandlerError> {
        let path = normalize_path(path);
        if path == "/index.md" {
            return self.index(None, None);
        }
        if path == "/log.md" {
            return Ok(self.log(None, None)?.document);
        }
        if let Some(id) = path
            .strip_prefix("/memories/")
            .and_then(|name| name.strip_suffix(".md"))
            .filter(|id| !id.contains('/'))
        {
            let rendered = OkfDocumentHandler::new(self.conn, self.scope.clone()).render(id)?;
            return Ok(VirtualEntry {
                path,
                kind: VirtualEntryKind::Document,
                read_only: false,
                content: rendered.text,
            });
        }
        if let Some(encoded) = path
            .strip_prefix("/types/")
            .and_then(|rest| rest.strip_suffix("/index.md"))
        {
            return self.index(Some(&percent_decode(encoded)?), None);
        }
        if let Some(encoded) = path
            .strip_prefix("/tags/")
            .and_then(|rest| rest.strip_suffix("/index.md"))
        {
            return self.index(None, Some(&percent_decode(encoded)?));
        }
        Err(HandlerError::InvalidTarget(path))
    }

    pub fn list(&self, path: &str) -> Result<Vec<VirtualEntrySummary>, HandlerError> {
        let path = normalize_path(path);
        match path.as_str() {
            "/" => Ok(vec![
                summary("/index.md", VirtualEntryKind::Index, true),
                summary("/log.md", VirtualEntryKind::Log, true),
                summary("/memories/", VirtualEntryKind::Directory, true),
                summary("/types/", VirtualEntryKind::Directory, true),
                summary("/tags/", VirtualEntryKind::Directory, true),
            ]),
            "/memories" | "/memories/" => self.list_memories(),
            "/types" | "/types/" => self.list_types(),
            "/tags" | "/tags/" => self.list_tags(),
            _ => Err(HandlerError::InvalidTarget(path)),
        }
    }

    pub fn index(
        &self,
        concept_type: Option<&str>,
        tag: Option<&str>,
    ) -> Result<VirtualEntry, HandlerError> {
        let rows = self.index_rows(concept_type, tag)?;
        let path = match (concept_type, tag) {
            (Some(value), None) => format!("/types/{}/index.md", percent_encode(value)),
            (None, Some(value)) => format!("/tags/{}/index.md", percent_encode(value)),
            _ => "/index.md".to_string(),
        };
        let mut text = format!("# Memory bundle index\n\nBundle: `{}`\n", self.uri());
        if let Some(value) = concept_type {
            text.push_str(&format!("\nType: `{value}`\n"));
        }
        if let Some(value) = tag {
            text.push_str(&format!("\nTag: `{value}`\n"));
        }
        if rows.is_empty() {
            text.push_str("\n_No concepts._\n");
        } else if concept_type.is_some() || tag.is_some() {
            text.push('\n');
            for row in rows {
                render_index_row(&mut text, &row);
            }
        } else {
            let mut groups: BTreeMap<String, Vec<IndexRow>> = BTreeMap::new();
            for row in rows {
                groups
                    .entry(row.concept_type.clone())
                    .or_default()
                    .push(row);
            }
            for (group, rows) in groups {
                text.push_str(&format!("\n## {group}\n\n"));
                for row in rows {
                    render_index_row(&mut text, &row);
                }
            }
        }
        Ok(VirtualEntry {
            path,
            kind: VirtualEntryKind::Index,
            read_only: true,
            content: text,
        })
    }

    pub fn log(
        &self,
        cursor: Option<usize>,
        limit: Option<usize>,
    ) -> Result<BundlePage, HandlerError> {
        let offset = cursor.unwrap_or(0);
        let limit = limit.unwrap_or(DEFAULT_LOG_LIMIT).clamp(1, MAX_LOG_LIMIT);
        let mut events = self.log_events()?;
        events.sort_by(|left, right| {
            right
                .at
                .cmp(&left.at)
                .then_with(|| right.memory_id.cmp(&left.memory_id))
                .then_with(|| right.revision.cmp(&left.revision))
        });
        let total = events.len();
        let selected = events
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        let next_cursor = (offset + selected.len() < total).then_some(offset + selected.len());
        let mut text = format!("# Memory bundle log\n\nBundle: `{}`\n", self.uri());
        let mut current_date = String::new();
        for event in selected {
            let date = event.at.get(..10).unwrap_or(event.at.as_str());
            if date != current_date {
                current_date = date.to_string();
                text.push_str(&format!("\n## {date}\n\n"));
            }
            let actor = event
                .actor
                .as_deref()
                .map(|actor| format!(" by `{actor}`"))
                .unwrap_or_default();
            if event.tombstone {
                text.push_str(&format!(
                    "- `{}` `{}`{actor} — deleted memory `{}`\n",
                    event.at, event.operation, event.memory_id
                ));
            } else {
                text.push_str(&format!(
                    "- `{}` [`{}`](/memories/{}.md#revision-{}) `{}`{actor}\n",
                    event.at,
                    short_id(&event.memory_id),
                    event.memory_id,
                    event.revision,
                    event.operation,
                ));
            }
        }
        if total == 0 {
            text.push_str("\n_No history._\n");
        }
        Ok(BundlePage {
            document: VirtualEntry {
                path: "/log.md".to_string(),
                kind: VirtualEntryKind::Log,
                read_only: true,
                content: text,
            },
            next_cursor,
        })
    }

    fn scope_predicate(&self, alias: &str) -> (String, Vec<Value>) {
        match &self.scope {
            BundleScope::Project(project) => (
                format!("{alias}.project = ?"),
                vec![Value::Text(project.clone())],
            ),
            BundleScope::Global => (
                format!("{alias}.project = ?"),
                vec![Value::Text(queries::GLOBAL_PROJECT_IDENT.to_string())],
            ),
            BundleScope::Unscoped => (format!("{alias}.project IS NULL"), Vec::new()),
        }
    }

    fn list_memories(&self) -> Result<Vec<VirtualEntrySummary>, HandlerError> {
        let (scope, values) = self.scope_predicate("m");
        let sql = format!(
            "SELECT m.id FROM memories m WHERE {scope} AND m.superseded_by IS NULL ORDER BY m.id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt
            .query_map(params_from_iter(values), |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids
            .into_iter()
            .map(|id| {
                summary(
                    &format!("/memories/{id}.md"),
                    VirtualEntryKind::Document,
                    false,
                )
            })
            .collect())
    }

    fn list_types(&self) -> Result<Vec<VirtualEntrySummary>, HandlerError> {
        let (scope, values) = self.scope_predicate("m");
        let sql = format!(
            "SELECT DISTINCT c.concept_type FROM memory_concepts c
             JOIN memories m ON m.id = c.memory_id
             WHERE {scope} AND m.superseded_by IS NULL ORDER BY c.concept_type"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let types = stmt
            .query_map(params_from_iter(values), |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(types
            .into_iter()
            .map(|value| {
                summary(
                    &format!("/types/{}/index.md", percent_encode(&value)),
                    VirtualEntryKind::Index,
                    true,
                )
            })
            .collect())
    }

    fn list_tags(&self) -> Result<Vec<VirtualEntrySummary>, HandlerError> {
        let (scope, values) = self.scope_predicate("m");
        let sql = format!(
            "SELECT m.tags FROM memories m WHERE {scope} AND m.superseded_by IS NULL
             AND m.tags IS NOT NULL ORDER BY m.id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let tags_json = stmt
            .query_map(params_from_iter(values), |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut tags = tags_json
            .iter()
            .flat_map(|raw| serde_json::from_str::<Vec<String>>(raw).unwrap_or_default())
            .collect::<Vec<_>>();
        tags.sort();
        tags.dedup();
        Ok(tags
            .into_iter()
            .map(|value| {
                summary(
                    &format!("/tags/{}/index.md", percent_encode(&value)),
                    VirtualEntryKind::Index,
                    true,
                )
            })
            .collect())
    }

    fn index_rows(
        &self,
        concept_type: Option<&str>,
        tag: Option<&str>,
    ) -> Result<Vec<IndexRow>, HandlerError> {
        let (scope, mut values) = self.scope_predicate("m");
        let mut sql = format!(
            "SELECT m.id, c.concept_type, c.title, c.description, m.content,
                    c.status, c.stale_after, m.tags
             FROM memories m JOIN memory_concepts c ON c.memory_id = m.id
             WHERE {scope} AND m.superseded_by IS NULL"
        );
        if let Some(value) = concept_type {
            sql.push_str(" AND c.concept_type = ?");
            values.push(Value::Text(value.to_string()));
        }
        sql.push_str(" ORDER BY c.concept_type, COALESCE(c.title, m.id), m.id");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(params_from_iter(values), |row| {
                let tags: Option<String> = row.get(7)?;
                Ok(IndexRow {
                    id: row.get(0)?,
                    concept_type: row.get(1)?,
                    title: row.get(2)?,
                    description: row.get(3)?,
                    body: row.get(4)?,
                    status: row.get(5)?,
                    stale_after: row.get(6)?,
                    tags: tags
                        .as_deref()
                        .and_then(|raw| serde_json::from_str(raw).ok())
                        .unwrap_or_default(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(tag) = tag {
            rows.retain(|row| row.tags.iter().any(|candidate| candidate == tag));
        }
        Ok(rows)
    }

    fn log_events(&self) -> Result<Vec<LogEvent>, HandlerError> {
        let (live_scope, live_values) = self.scope_predicate("m");
        let live_sql = format!(
            "SELECT r.memory_id, r.revision, r.operation, r.actor, r.created_at
             FROM memory_revisions r JOIN memories m ON m.id = r.memory_id
             WHERE {live_scope}"
        );
        let mut stmt = self.conn.prepare(&live_sql)?;
        let mut events = stmt
            .query_map(params_from_iter(live_values), |row| {
                Ok(LogEvent {
                    memory_id: row.get(0)?,
                    revision: row.get(1)?,
                    operation: row.get(2)?,
                    actor: row.get(3)?,
                    at: row.get(4)?,
                    tombstone: false,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let (tombstone_scope, tombstone_values) = match &self.scope {
            BundleScope::Project(project) => (
                "project = ?".to_string(),
                vec![Value::Text(project.clone())],
            ),
            BundleScope::Global => ("scope = 'global'".to_string(), Vec::new()),
            BundleScope::Unscoped => ("scope = 'unscoped'".to_string(), Vec::new()),
        };
        let tombstone_sql = format!(
            "SELECT memory_id, last_revision, actor, deleted_at
             FROM memory_concept_tombstones WHERE {tombstone_scope}"
        );
        let mut stmt = self.conn.prepare(&tombstone_sql)?;
        events.extend(
            stmt.query_map(params_from_iter(tombstone_values), |row| {
                Ok(LogEvent {
                    memory_id: row.get(0)?,
                    revision: row.get(1)?,
                    operation: "forget".to_string(),
                    actor: row.get(2)?,
                    at: row.get(3)?,
                    tombstone: true,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?,
        );
        Ok(events)
    }
}

#[derive(Debug)]
struct IndexRow {
    id: String,
    concept_type: String,
    title: Option<String>,
    description: Option<String>,
    body: String,
    status: String,
    stale_after: Option<String>,
    tags: Vec<String>,
}

#[derive(Debug)]
struct LogEvent {
    memory_id: String,
    revision: i64,
    operation: String,
    actor: Option<String>,
    at: String,
    tombstone: bool,
}

fn render_index_row(text: &mut String, row: &IndexRow) {
    let label = row.title.as_deref().unwrap_or_else(|| short_id(&row.id));
    let summary = row
        .description
        .as_deref()
        .unwrap_or(&row.body)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let preview = bounded_preview(&summary, PREVIEW_CHARS);
    let stale = row
        .stale_after
        .as_deref()
        .is_some_and(|date| date < chrono::Utc::now().date_naive().to_string().as_str());
    let stale_label = if stale { ", stale" } else { "" };
    text.push_str(&format!(
        "- [{label}](/memories/{}.md) — {} (`{}{stale_label}`)\n",
        row.id, preview, row.status
    ));
}

fn bounded_preview(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let preview = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn summary(path: &str, kind: VirtualEntryKind, read_only: bool) -> VirtualEntrySummary {
    VirtualEntrySummary {
        path: path.to_string(),
        kind,
        read_only,
    }
}

fn normalize_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::Memory, open_database, queries};

    fn insert(conn: &Connection, id: &str, project: Option<&str>, memory_type: &str) {
        let mut memory = Memory::new(
            format!("A body for {id} with a bounded preview."),
            Some(vec!["shared".to_string(), format!("tag-{memory_type}")]),
            project.map(str::to_string),
            None,
            None,
            Some(memory_type.to_string()),
        );
        memory.id = id.to_string();
        queries::insert_memory(conn, &memory).expect("insert");
    }

    #[test]
    fn indexes_and_lists_are_deterministic_and_scope_isolated() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        insert(&conn, "aaaaaaaa-0000", Some("alpha"), "project");
        insert(&conn, "bbbbbbbb-0000", Some("beta"), "reference");
        insert(
            &conn,
            "cccccccc-0000",
            Some(queries::GLOBAL_PROJECT_IDENT),
            "user",
        );
        insert(&conn, "dddddddd-0000", None, "feedback");
        queries::set_working_context(&conn, "alpha", "never indexed").expect("working context");

        let alpha = OkfBundleHandler::new(&conn, BundleScope::Project("alpha".to_string()));
        let root = alpha.read("/index.md").expect("root index");
        assert!(root.read_only);
        assert!(root.content.contains("aaaaaaaa-0000"));
        assert!(!root.content.contains("bbbbbbbb-0000"));
        assert!(!root.content.contains("never indexed"));
        let memories = alpha.list("/memories").expect("list");
        assert_eq!(memories.len(), 1);
        assert!(!memories[0].read_only);
        let type_index = alpha
            .read("/types/Agent%20Memory%2Fproject/index.md")
            .expect("type index");
        assert!(type_index.content.contains("aaaaaaaa-0000"));
        let tag_index = alpha.read("/tags/shared/index.md").expect("tag index");
        assert!(tag_index.content.contains("aaaaaaaa-0000"));

        let global = OkfBundleHandler::new(&conn, BundleScope::Global);
        assert!(global
            .read("/index.md")
            .expect("global")
            .content
            .contains("cccccccc-0000"));
        let unscoped = OkfBundleHandler::new(&conn, BundleScope::Unscoped);
        assert!(unscoped
            .read("/index.md")
            .expect("unscoped")
            .content
            .contains("dddddddd-0000"));
    }

    #[test]
    fn log_is_paginated_and_includes_tombstones() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        insert(&conn, "aaaaaaaa-1111", Some("alpha"), "user");
        queries::update_content(&conn, "aaaaaaaa-1111", "updated", None, None).expect("update");
        insert(&conn, "bbbbbbbb-1111", Some("alpha"), "user");
        queries::delete_memory(&conn, "bbbbbbbb-1111").expect("forget");

        let bundle = OkfBundleHandler::new(&conn, BundleScope::Project("alpha".to_string()));
        let first = bundle.log(None, Some(1)).expect("first page");
        assert_eq!(first.next_cursor, Some(1));
        assert!(first.document.content.contains("## 20"));
        let rest = bundle.log(first.next_cursor, Some(10)).expect("rest");
        assert!(
            rest.document.content.contains("forget") || first.document.content.contains("forget")
        );
        assert!(matches!(
            bundle.read("/memories/bbbbbbbb-1111.md"),
            Err(HandlerError::Memory(crate::error::MemoryError::NotFound(_)))
        ));
    }
}
