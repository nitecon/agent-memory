use rusqlite::{params, Connection};
use std::collections::HashMap;

use crate::error::MemoryError;

/// Search memories using SQLite FTS5 with BM25 ranking.
/// The FTS5 virtual table is kept in sync via triggers on the memories table.
pub fn search_bm25(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<(String, f32)>, MemoryError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.chars().count() > 4_096 || trimmed.contains('\0') {
        return Ok(Vec::new());
    }
    let query = bounded_fts_query(trimmed);

    // Search the derived OKF segment corpus. Multiple segment hits collapse
    // deterministically to one resource using its best score, preventing a
    // long document from occupying adjacent result slots.
    let mut stmt = conn.prepare(
        "SELECT s.memory_id, bm25(memory_segments_fts) AS score
         FROM memory_segments_fts s
         JOIN memories m ON m.id = s.memory_id
         JOIN memory_concepts c ON c.memory_id = m.id
         WHERE memory_segments_fts MATCH ?1
           AND m.superseded_by IS NULL
           AND c.status <> 'deprecated'
         ORDER BY score, s.memory_id, CAST(s.segment_no AS INTEGER)
         LIMIT ?2",
    )?;

    let segment_limit = limit.saturating_mul(8).max(limit);
    let rows = match stmt.query_map(params![query, segment_limit as i64], |row| {
        let id: String = row.get(0)?;
        let score: f64 = row.get(1)?;
        Ok((id, (-score) as f32)) // Negate: FTS5 bm25() returns negative values (closer to 0 = better)
    }) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()), // Malformed FTS5 query
    };

    let mut best = HashMap::<String, f32>::new();
    for row in rows {
        match row {
            Ok((id, score)) => {
                best.entry(id)
                    .and_modify(|current| *current = current.max(score))
                    .or_insert(score);
            }
            Err(_) => break,
        }
    }
    let mut results = best.into_iter().collect::<Vec<_>>();
    results.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    results.truncate(limit);
    Ok(results)
}

fn bounded_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .take(64)
        .map(|token| {
            let operator = matches!(token, "AND" | "OR" | "NOT" | "NEAR");
            if operator || token.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
                token.to_string()
            } else {
                format!("\"{}\"", token.replace('"', "\"\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_fts_syntax_and_oversized_queries_fail_closed() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&conn).unwrap();
        for payload in [
            "\"unterminated",
            "foo OR (bar",
            "NEAR(thing, 999999999)",
            "*:* ^ DROP TABLE memories; --",
            "nul\0suffix",
        ] {
            assert!(search_bm25(&conn, payload, 10).unwrap().is_empty());
        }
        assert!(search_bm25(&conn, &"x".repeat(4_097), 10)
            .unwrap()
            .is_empty());
        let table: String = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'memories'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table, "memories");
    }

    #[test]
    fn fts_token_budget_is_deterministic() {
        let input = (0..100)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let bounded = bounded_fts_query(&input);
        assert_eq!(bounded.split_whitespace().count(), 64);
        assert!(bounded.ends_with("word63"));
    }
}
