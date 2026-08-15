use std::collections::{BTreeSet, VecDeque};

use rusqlite::{params, Connection};

use crate::error::MemoryError;

const HARD_MAX_DEPTH: usize = 8;
const HARD_MAX_FAN_OUT: usize = 100;
const HARD_MAX_RESULTS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraversalOptions {
    pub direction: Direction,
    pub relations: BTreeSet<String>,
    pub max_depth: usize,
    pub fan_out: usize,
    pub max_results: usize,
}

impl Default for TraversalOptions {
    fn default() -> Self {
        Self {
            direction: Direction::Outgoing,
            relations: BTreeSet::new(),
            max_depth: 2,
            fan_out: 25,
            max_results: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphStep {
    pub from: String,
    pub to: String,
    pub relation: String,
    pub reversed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphPath {
    pub nodes: Vec<String>,
    pub steps: Vec<GraphStep>,
    pub cycle: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDiagnostic {
    pub source: String,
    pub reference: String,
    pub relation: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraversalResult {
    pub paths: Vec<GraphPath>,
    pub diagnostics: Vec<GraphDiagnostic>,
    pub truncated: bool,
}

#[derive(Debug)]
struct Edge {
    src: String,
    dst: Option<String>,
    reference: String,
    relation: String,
}

pub fn traverse(
    conn: &Connection,
    roots: &[String],
    options: &TraversalOptions,
) -> Result<TraversalResult, MemoryError> {
    let depth_limit = options.max_depth.min(HARD_MAX_DEPTH);
    let fan_out = options.fan_out.clamp(1, HARD_MAX_FAN_OUT);
    let result_limit = options.max_results.clamp(1, HARD_MAX_RESULTS);
    let mut queue = VecDeque::new();
    for root in roots {
        queue.push_back(GraphPath {
            nodes: vec![root.clone()],
            steps: Vec::new(),
            cycle: false,
        });
    }
    let mut paths = Vec::new();
    let mut diagnostics = Vec::new();
    let mut truncated = false;

    while let Some(path) = queue.pop_front() {
        if path.steps.len() >= depth_limit {
            continue;
        }
        let node = path.nodes.last().expect("paths always contain a root");
        let edges = adjacent(conn, node, options.direction, fan_out)?;
        if edges.len() == fan_out {
            truncated = true;
        }
        for (edge, reversed) in edges {
            if !options.relations.is_empty() && !options.relations.contains(&edge.relation) {
                continue;
            }
            let neighbor = if reversed {
                Some(edge.src.clone())
            } else {
                edge.dst.clone()
            };
            let Some(neighbor) = neighbor else {
                diagnostics.push(GraphDiagnostic {
                    source: node.clone(),
                    reference: edge.reference,
                    relation: edge.relation,
                    message: "unresolved or external relationship".to_string(),
                });
                continue;
            };
            let cycle = path.nodes.contains(&neighbor);
            let mut next = path.clone();
            next.nodes.push(neighbor.clone());
            next.steps.push(GraphStep {
                from: node.clone(),
                to: neighbor,
                relation: edge.relation,
                reversed,
            });
            next.cycle = cycle;
            paths.push(next.clone());
            if paths.len() >= result_limit {
                truncated = true;
                return Ok(TraversalResult {
                    paths,
                    diagnostics,
                    truncated,
                });
            }
            if !cycle {
                queue.push_back(next);
            }
        }
    }
    diagnostics.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.relation.cmp(&right.relation))
            .then_with(|| left.reference.cmp(&right.reference))
    });
    diagnostics.dedup();
    Ok(TraversalResult {
        paths,
        diagnostics,
        truncated,
    })
}

fn adjacent(
    conn: &Connection,
    node: &str,
    direction: Direction,
    limit: usize,
) -> Result<Vec<(Edge, bool)>, MemoryError> {
    let mut edges = Vec::new();
    if matches!(direction, Direction::Outgoing | Direction::Both) {
        let mut stmt = conn.prepare(
            "SELECT src_memory_id, dst_memory_id, dst_ref, relation
             FROM memory_relationships WHERE src_memory_id = ?1
             ORDER BY relation, dst_ref, producer, COALESCE(ordinal, -1), id LIMIT ?2",
        )?;
        edges.extend(
            stmt.query_map(params![node, limit as i64], |row| {
                Ok((
                    Edge {
                        src: row.get(0)?,
                        dst: row.get(1)?,
                        reference: row.get(2)?,
                        relation: row.get(3)?,
                    },
                    false,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?,
        );
    }
    if matches!(direction, Direction::Incoming | Direction::Both) && edges.len() < limit {
        let remaining = limit - edges.len();
        let mut stmt = conn.prepare(
            "SELECT src_memory_id, dst_memory_id, dst_ref, relation
             FROM memory_relationships WHERE dst_memory_id = ?1
             ORDER BY relation, src_memory_id, producer, COALESCE(ordinal, -1), id LIMIT ?2",
        )?;
        edges.extend(
            stmt.query_map(params![node, remaining as i64], |row| {
                Ok((
                    Edge {
                        src: row.get(0)?,
                        dst: row.get(1)?,
                        reference: row.get(2)?,
                        relation: row.get(3)?,
                    },
                    true,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?,
        );
    }
    Ok(edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts;
    use crate::db::{models::Memory, open_database, queries};

    fn insert(conn: &Connection, id: &str) {
        let mut memory = Memory::new(id.to_string(), None, None, None, None, None);
        memory.id = id.to_string();
        queries::insert_memory(conn, &memory).expect("insert");
    }

    fn edge(conn: &Connection, from: &str, to: Option<&str>, reference: &str, relation: &str) {
        concepts::insert_relationship(conn, from, to, reference, relation, 1, None).expect("edge");
    }

    #[test]
    fn traversal_is_bounded_deterministic_and_reports_cycles_and_unresolved_edges() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        for id in ["a", "b", "c"] {
            insert(&conn, id);
        }
        edge(&conn, "a", Some("b"), "b", "links_to");
        edge(&conn, "a", None, "https://example.invalid", "cites");
        edge(&conn, "b", Some("c"), "c", "links_to");
        edge(&conn, "c", Some("a"), "a", "links_to");

        let result = traverse(
            &conn,
            &["a".to_string()],
            &TraversalOptions {
                max_depth: 3,
                ..TraversalOptions::default()
            },
        )
        .expect("traverse");
        assert!(result
            .paths
            .iter()
            .any(|path| path.nodes == ["a", "b", "c"]));
        assert!(result.paths.iter().any(|path| path.cycle));
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].reference, "https://example.invalid");
    }

    #[test]
    fn direction_relation_and_result_limits_are_enforced_without_sql_interpolation() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        for id in ["a", "b", "c", "d"] {
            insert(&conn, id);
        }
        edge(&conn, "a", Some("b"), "b", "links_to");
        edge(&conn, "c", Some("b"), "b", "cites");
        edge(
            &conn,
            "d",
            Some("b"),
            "b",
            "links_to'); DROP TABLE memories;--",
        );

        let incoming = traverse(
            &conn,
            &["b".to_string()],
            &TraversalOptions {
                direction: Direction::Incoming,
                relations: BTreeSet::from(["links_to".to_string()]),
                max_results: 1,
                ..TraversalOptions::default()
            },
        )
        .expect("incoming");
        assert_eq!(incoming.paths.len(), 1);
        assert_eq!(incoming.paths[0].nodes, ["b", "a"]);
        assert!(incoming.truncated);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row
                .get::<_, i64>(0))
                .expect("memories still exist"),
            4
        );
    }

    #[test]
    fn adversarial_fan_out_is_capped() {
        let conn = open_database(std::path::Path::new(":memory:")).expect("database");
        insert(&conn, "root");
        for index in 0..110 {
            let id = format!("node-{index:03}");
            insert(&conn, &id);
            edge(&conn, "root", Some(&id), &id, "links_to");
        }
        let result = traverse(
            &conn,
            &["root".to_string()],
            &TraversalOptions {
                max_depth: 99,
                fan_out: 10_000,
                max_results: 10_000,
                ..TraversalOptions::default()
            },
        )
        .expect("traverse");
        assert_eq!(result.paths.len(), HARD_MAX_FAN_OUT);
        assert!(result.truncated);
    }
}
