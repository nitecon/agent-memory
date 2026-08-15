//! Rebuildable lexical projection of canonical OKF concepts.
//!
//! This is deliberately derived state: SQLite memories/concepts remain canonical,
//! while rows here can be dropped and regenerated at any time.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::MemoryError;

const SHORT_BODY_CHARS: usize = 1_200;
const MAX_SEGMENT_CHARS: usize = 1_600;
const MAX_SEGMENTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSegment {
    pub ordinal: usize,
    pub heading_path: Option<String>,
    pub body: String,
}

pub fn segment_markdown(body: &str) -> Vec<SearchSegment> {
    if body.chars().count() <= SHORT_BODY_CHARS {
        return vec![SearchSegment {
            ordinal: 0,
            heading_path: None,
            body: body.to_string(),
        }];
    }

    let mut sections: Vec<(Option<String>, String)> = Vec::new();
    let mut headings: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_path = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
        let is_heading = (1..=6).contains(&hashes)
            && trimmed.chars().nth(hashes).is_some_and(char::is_whitespace);
        if is_heading {
            if !current.trim().is_empty() {
                sections.push((current_path.take(), current.trim().to_string()));
                current.clear();
            }
            headings.truncate(hashes.saturating_sub(1));
            headings.push(trimmed[hashes..].trim().to_string());
            current_path = Some(headings.join(" / "));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        sections.push((current_path, current.trim().to_string()));
    }
    if sections.is_empty() {
        sections.push((None, body.to_string()));
    }

    let mut output = Vec::new();
    for (heading_path, section) in sections {
        let chars = section.chars().collect::<Vec<_>>();
        for chunk in chars.chunks(MAX_SEGMENT_CHARS) {
            if output.len() >= MAX_SEGMENTS {
                return output;
            }
            output.push(SearchSegment {
                ordinal: output.len(),
                heading_path: heading_path.clone(),
                body: chunk.iter().collect(),
            });
        }
    }
    output
}

pub fn rebuild_memory_segments(conn: &Connection, memory_id: &str) -> Result<(), MemoryError> {
    conn.execute(
        "DELETE FROM memory_segments_fts WHERE memory_id = ?1",
        params![memory_id],
    )?;
    let row = conn
        .query_row(
            "SELECT m.content, m.tags, c.title, c.description, c.concept_type
             FROM memories m JOIN memory_concepts c ON c.memory_id = m.id
             WHERE m.id = ?1",
            params![memory_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((body, tags_json, title, description, concept_type)) = row else {
        return Ok(());
    };
    let tags = tags_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default()
        .join(" ");
    for segment in segment_markdown(&body) {
        let searchable = [
            title.as_deref().unwrap_or_default(),
            description.as_deref().unwrap_or_default(),
            &concept_type,
            &tags,
            segment.heading_path.as_deref().unwrap_or_default(),
            &segment.body,
        ]
        .join("\n");
        conn.execute(
            "INSERT INTO memory_segments_fts (
                 memory_id, segment_no, heading_path, searchable
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                memory_id,
                segment.ordinal as i64,
                segment.heading_path,
                searchable
            ],
        )?;
    }
    Ok(())
}

pub fn rebuild_all_segments(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute("DELETE FROM memory_segments_fts", [])?;
    let ids = {
        let mut stmt = conn.prepare("SELECT id FROM memories ORDER BY id")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids
    };
    for id in ids {
        rebuild_memory_segments(conn, &id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_body_is_one_segment_and_long_markdown_is_bounded_by_headings() {
        assert_eq!(segment_markdown("short").len(), 1);
        let body = format!(
            "# One\n{}\n## Two\n{}",
            "a".repeat(1_400),
            "b".repeat(2_000)
        );
        let segments = segment_markdown(&body);
        assert!(segments.len() >= 3);
        assert!(segments.len() <= MAX_SEGMENTS);
        assert!(segments
            .iter()
            .all(|segment| segment.body.chars().count() <= MAX_SEGMENT_CHARS));
        assert!(segments
            .iter()
            .any(|segment| { segment.heading_path.as_deref() == Some("One / Two") }));
    }
}
