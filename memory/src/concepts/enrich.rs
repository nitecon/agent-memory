//! Deterministic OKF enrichment.
//!
//! Dream is the only writer that can supply *judgement* — what a memory means,
//! whether two memories contradict, which one survives. It is also expensive,
//! scheduled, and model-dependent. Everything about a concept that follows from
//! the body's own structure needs none of that, and making Dream the only
//! source of OKF metadata left `title`, `description` and `sources` empty on
//! every memory in the store.
//!
//! This module derives those fields structurally, so recall gets useful
//! descriptors immediately and Dream's model time is spent on triage,
//! deduplication and rewriting rather than on restating the first line.
//!
//! Contract:
//!
//! - **Never overwrites.** Only absent fields are filled, so a human or Dream
//!   value always wins and re-running is idempotent.
//! - **Marked as derived.** What was inferred is recorded under the
//!   `x-agent-memory-derived` extension, so a later writer can tell a guessed
//!   title from an authored one and replace it without hesitation.
//! - **Never invents.** When the body has no headline shape, the field stays
//!   absent rather than receiving a mangled prefix.
//! - **Not a semantic change.** Enrichment restates what the body already says,
//!   so it does not invalidate verification the way an edit to the claim does.
//! - **No inference, network, or filesystem work**, matching the rest of this
//!   write boundary.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::MemoryError;

/// Producer/actor recorded for derived metadata. Versioned so a later rule
/// change can be identified — and re-derived — without guessing.
pub const ENRICHER: &str = "agent-memory/enrich@1";

/// Extension key holding the list of structurally derived field names.
pub const DERIVED_EXTENSION_KEY: &str = "x-agent-memory-derived";

const TITLE_MAX_CHARS: usize = 120;
const TITLE_MIN_CHARS: usize = 8;
const DESCRIPTION_MAX_CHARS: usize = 240;
const MAX_DERIVED_SOURCES: usize = 8;
/// Upper bound on how much of a body is scanned. Bodies are normally a few
/// hundred bytes; this only exists so a pathological paste cannot make recall
/// pay an unbounded scan.
const MAX_SCAN_BYTES: usize = 64 * 1024;

/// File extensions treated as citable resources when they appear in a
/// directory-qualified path. Deliberately narrow: a bare `mod.rs` is noise,
/// `crates/gateway/src/db.rs` is provenance.
const RESOURCE_EXTENSIONS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "rb", "c", "h", "cpp", "md", "yaml", "yml",
    "toml", "json", "sh", "sql", "tf", "proto",
];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Derived {
    pub title: Option<String>,
    pub description: Option<String>,
    pub sources: Vec<String>,
}

impl Derived {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none() && self.sources.is_empty()
    }
}

/// Derive OKF descriptors from a body. Pure — the interesting behaviour lives
/// here so the rules are testable without a database.
///
/// Titling follows the shape memories are actually written in: a `Subject:
/// claim` headline on the first line, with supporting bullets underneath. The
/// subject becomes the title and the claim becomes the description, which is
/// why the split is preferred over simply truncating the first line.
pub fn derive(body: &str) -> Derived {
    let scan = bounded_scan(body);
    let mut lines = scan
        .lines()
        .map(strip_markup)
        .filter(|line| !line.is_empty());
    let Some(headline) = lines.next() else {
        return Derived::default();
    };
    let headline = collapse_whitespace(headline);

    let (title, description) = split_headline(&headline, lines.next());

    Derived {
        title,
        description,
        sources: derive_sources(scan),
    }
}

/// Split a headline into `(title, description)`.
///
/// Three shapes, in order of preference:
/// 1. `Subject: claim` — subject titles, claim describes.
/// 2. A first line short enough to title on its own — the following line, if
///    any, describes.
/// 3. Anything longer — no title at all (a truncated prefix reads as a
///    mistake), and the line itself becomes the description.
fn split_headline(headline: &str, next_line: Option<&str>) -> (Option<String>, Option<String>) {
    if let Some((subject, claim)) = headline.split_once(':') {
        let subject = subject.trim();
        let claim = claim.trim();
        // A subject that already contains a sentence break is prose that
        // happens to precede a colon, not a heading. Titling on it produces
        // things like "X falls out of sync with live systems. Example".
        let is_subject = !subject.contains(". ")
            && (TITLE_MIN_CHARS..=TITLE_MAX_CHARS).contains(&subject.chars().count());
        if is_subject && !claim.is_empty() {
            return (
                Some(subject.to_string()),
                Some(truncate_words(claim, DESCRIPTION_MAX_CHARS)),
            );
        }
    }

    if headline.chars().count() <= TITLE_MAX_CHARS {
        let description = next_line
            .map(strip_markup)
            .map(collapse_whitespace)
            .filter(|line| !line.is_empty())
            .map(|line| truncate_words(&line, DESCRIPTION_MAX_CHARS));
        return (Some(headline.to_string()), description);
    }

    (None, Some(truncate_words(headline, DESCRIPTION_MAX_CHARS)))
}

/// Collect citable resources: absolute URLs and directory-qualified source
/// paths, in order of appearance, deduplicated, bounded.
fn derive_sources(scan: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for token in scan.split_whitespace() {
        let token = trim_delimiters(token);
        let citable = token.starts_with("http://")
            || token.starts_with("https://")
            || is_resource_path(token);
        if citable && !found.iter().any(|existing| existing == token) {
            found.push(token.to_string());
            if found.len() >= MAX_DERIVED_SOURCES {
                break;
            }
        }
    }
    found
}

/// A path qualifies when it names a directory *and* a known source extension.
/// The directory requirement is what keeps prose mentions of `mod.rs` out.
fn is_resource_path(token: &str) -> bool {
    if !token.contains('/') || token.contains("://") || token.len() > 200 {
        return false;
    }
    let Some((_, extension)) = token.rsplit_once('.') else {
        return false;
    };
    if !RESOURCE_EXTENSIONS.contains(&extension) {
        return false;
    }
    let last = token.rsplit('/').next().unwrap_or_default();
    !last.is_empty()
        && !last.starts_with('.')
        && token
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@'))
}

/// Strip characters that commonly wrap a token in prose without being part of
/// it. Trailing sentence punctuation is removed; a trailing slash is kept
/// because it is meaningful in a URL.
fn trim_delimiters(token: &str) -> &str {
    token
        .trim_matches(['`', '"', '\'', '(', ')', '[', ']', '<', '>'])
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
}

/// Remove leading Markdown list/heading/quote markers.
fn strip_markup(line: &str) -> &str {
    line.trim()
        .trim_start_matches(['#', '>', '*', '-', '+'])
        .trim()
}

fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

/// Truncate on a word boundary, marking the cut. Unlike the title rules a
/// shortened description is still useful, so this trims rather than declining.
fn truncate_words(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let prefix: String = value.chars().take(max_chars).collect();
    let cut = match prefix.rsplit_once(' ') {
        Some((head, _)) if head.chars().count() >= max_chars / 2 => head,
        _ => prefix.trim_end(),
    };
    format!(
        "{}…",
        cut.trim_end_matches(|c: char| c.is_whitespace() || c == ',')
    )
}

/// Cut the body at a UTF-8 boundary at or below the scan bound.
fn bounded_scan(body: &str) -> &str {
    if body.len() <= MAX_SCAN_BYTES {
        return body;
    }
    let mut end = MAX_SCAN_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

/// Fill absent OKF descriptors on an existing concept row.
///
/// Assumes the caller owns the surrounding transaction/revision. Returns the
/// field names that were filled, so callers can record them.
pub fn apply_derived(
    conn: &Connection,
    memory_id: &str,
    derived: &Derived,
) -> Result<Vec<String>, MemoryError> {
    let current = conn
        .query_row(
            "SELECT title, description, extensions_json FROM memory_concepts WHERE memory_id = ?1",
            params![memory_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((title, description, extensions_json)) = current else {
        return Ok(Vec::new());
    };

    let mut filled: Vec<String> = Vec::new();
    if title.is_none() {
        if let Some(derived_title) = derived.title.as_deref() {
            conn.execute(
                "UPDATE memory_concepts SET title = ?1 WHERE memory_id = ?2",
                params![derived_title, memory_id],
            )?;
            filled.push("title".to_string());
        }
    }
    if description.is_none() {
        if let Some(derived_description) = derived.description.as_deref() {
            conn.execute(
                "UPDATE memory_concepts SET description = ?1 WHERE memory_id = ?2",
                params![derived_description, memory_id],
            )?;
            filled.push("description".to_string());
        }
    }

    if !derived.sources.is_empty() {
        let mut next_ordinal: i64 = conn.query_row(
            "SELECT COALESCE(MAX(ordinal), -1) + 1 FROM memory_sources WHERE memory_id = ?1",
            params![memory_id],
            |row| row.get(0),
        )?;
        let mut existing_keys =
            conn.prepare("SELECT source_key FROM memory_sources WHERE memory_id = ?1")?;
        let known = existing_keys
            .query_map(params![memory_id], |row| row.get::<_, String>(0))?
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        let mut added = false;
        for resource in &derived.sources {
            let key = source_key(resource);
            if known.contains(&key) {
                continue;
            }
            conn.execute(
                "INSERT OR IGNORE INTO memory_sources (
                     memory_id, source_key, ordinal, resource, metadata_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    memory_id,
                    key,
                    next_ordinal,
                    resource,
                    format!("{{\"derived_by\":\"{ENRICHER}\"}}"),
                ],
            )?;
            next_ordinal += 1;
            added = true;
        }
        if added {
            filled.push("sources".to_string());
        }
    }

    if !filled.is_empty() {
        record_derived_fields(conn, memory_id, &extensions_json, &filled)?;
    }
    Ok(filled)
}

/// Stable per-resource key so reordering never reattributes a claim.
fn source_key(resource: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(resource.as_bytes());
    format!("auto-{:x}", digest)[..17].to_string()
}

/// Record which fields were structurally derived, merging with any prior
/// record so an earlier enrichment's marks are not dropped.
fn record_derived_fields(
    conn: &Connection,
    memory_id: &str,
    extensions_json: &str,
    filled: &[String],
) -> Result<(), MemoryError> {
    let mut extensions: serde_json::Value =
        serde_json::from_str(extensions_json).unwrap_or_else(|_| serde_json::json!({}));
    if !extensions.is_object() {
        extensions = serde_json::json!({});
    }
    let mut fields: Vec<String> = extensions
        .get(DERIVED_EXTENSION_KEY)
        .and_then(|entry| entry.get("fields"))
        .and_then(|entry| entry.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for field in filled {
        if !fields.contains(field) {
            fields.push(field.clone());
        }
    }
    fields.sort();
    extensions[DERIVED_EXTENSION_KEY] = serde_json::json!({
        "by": ENRICHER,
        "fields": fields,
    });
    conn.execute(
        "UPDATE memory_concepts SET extensions_json = ?1 WHERE memory_id = ?2",
        params![extensions.to_string(), memory_id],
    )?;
    Ok(())
}

/// Derive from `body` and fill absent descriptors, without revision handling.
///
/// For use inside a write that is already minting a revision, so the derived
/// metadata lands in that revision's snapshot instead of a follow-up one.
pub fn derive_into(conn: &Connection, memory_id: &str, body: &str) -> Result<(), MemoryError> {
    apply_derived(conn, memory_id, &derive(body))?;
    Ok(())
}

/// Enrich one stored memory, creating a revision only when something was
/// actually filled.
///
/// `clear_verification` is false on purpose: derived descriptors restate what
/// the body already claims, so a prior human attestation of that claim still
/// holds. Only a change to the claim itself invalidates verification.
pub fn enrich(conn: &Connection, memory_id: &str) -> Result<Vec<String>, MemoryError> {
    let body = conn
        .query_row(
            "SELECT content FROM memories WHERE id = ?1",
            params![memory_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(body) = body else {
        return Ok(Vec::new());
    };
    let derived = derive(&body);
    if derived.is_empty() {
        return Ok(Vec::new());
    }

    let mut filled = Vec::new();
    super::mutate(
        conn,
        memory_id,
        "enrich",
        Some(ENRICHER),
        None,
        false,
        |_| {
            filled = apply_derived(conn, memory_id, &derived)?;
            Ok(())
        },
    )?;
    Ok(filled)
}

/// Report which fields enrichment would fill, without writing anything.
///
/// Mirrors [`enrich`]'s no-overwrite rule so a dry run reports what would
/// actually land rather than everything the body can yield.
pub fn preview(conn: &Connection, memory_id: &str) -> Result<Vec<String>, MemoryError> {
    let current = conn
        .query_row(
            "SELECT m.content, c.title, c.description
             FROM memories m JOIN memory_concepts c ON c.memory_id = m.id
             WHERE m.id = ?1",
            params![memory_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((body, title, description)) = current else {
        return Ok(Vec::new());
    };
    let derived = derive(&body);
    let mut filled = Vec::new();
    if title.is_none() && derived.title.is_some() {
        filled.push("title".to_string());
    }
    if description.is_none() && derived.description.is_some() {
        filled.push("description".to_string());
    }
    if !derived.sources.is_empty() {
        let mut stmt =
            conn.prepare("SELECT source_key FROM memory_sources WHERE memory_id = ?1")?;
        let known = stmt
            .query_map(params![memory_id], |row| row.get::<_, String>(0))?
            .collect::<Result<std::collections::HashSet<_>, _>>()?;
        if derived
            .sources
            .iter()
            .any(|resource| !known.contains(&source_key(resource)))
        {
            filled.push("sources".to_string());
        }
    }
    Ok(filled)
}

/// Best-effort enrichment for read paths.
///
/// Recall must return even when the store is locked by a concurrent writer or
/// opened read-only, so failures are swallowed rather than propagated. This is
/// the same degradation contract the reranker uses.
pub fn enrich_quietly(conn: &Connection, memory_id: &str) {
    if let Err(error) = enrich(conn, memory_id) {
        tracing::debug!(id = %memory_id, %error, "structural enrichment skipped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_claim_headline_splits_into_title_and_description() {
        let derived = derive(
            "Unreal UCX/Interchange collision: export constraints and Blender tooling\n\
             - Interchange imports static-mesh collision by exact display name match\n",
        );
        assert_eq!(
            derived.title.as_deref(),
            Some("Unreal UCX/Interchange collision")
        );
        assert_eq!(
            derived.description.as_deref(),
            Some("export constraints and Blender tooling")
        );
    }

    #[test]
    fn short_headline_without_a_subject_titles_itself_and_describes_from_the_next_line() {
        let derived = derive("Rotate the signing key quarterly\n- Owner is the platform team\n");
        assert_eq!(
            derived.title.as_deref(),
            Some("Rotate the signing key quarterly")
        );
        assert_eq!(
            derived.description.as_deref(),
            Some("Owner is the platform team")
        );
    }

    #[test]
    fn long_unstructured_headline_yields_a_description_but_never_a_mangled_title() {
        let body = format!("{} and then some more text after it", "word ".repeat(80));
        let derived = derive(&body);
        assert_eq!(derived.title, None, "a truncated prefix is not a title");
        let description = derived.description.expect("description");
        assert!(description.ends_with('…'));
        assert!(description.chars().count() <= DESCRIPTION_MAX_CHARS + 1);
    }

    #[test]
    fn a_colon_that_does_not_form_a_subject_falls_through_to_the_line_rules() {
        // Leading fragment is too short to be a subject.
        let derived = derive("v2: shipped the migration runner\n");
        assert_eq!(
            derived.title.as_deref(),
            Some("v2: shipped the migration runner")
        );
        assert_eq!(derived.description, None);
    }

    #[test]
    fn sources_collect_urls_and_directory_qualified_paths_only() {
        let derived = derive(
            "Gateway routing: deferred constraints\n\
             - see crates/gateway/src/routes.rs and crates/gateway/src/db.rs\n\
             - spec at https://example.invalid/spec.html.\n\
             - mod.rs alone is noise, and so is a bare word\n",
        );
        assert_eq!(
            derived.sources,
            vec![
                "crates/gateway/src/routes.rs",
                "crates/gateway/src/db.rs",
                "https://example.invalid/spec.html",
            ]
        );
    }

    #[test]
    fn derivation_is_bounded_and_deduplicated() {
        let mut body = String::from("Paths: many\n");
        for index in 0..40 {
            body.push_str(&format!("- src/a{index}/file.rs repeated src/a0/file.rs\n"));
        }
        let derived = derive(&body);
        assert_eq!(derived.sources.len(), MAX_DERIVED_SOURCES);
        let unique = derived
            .sources
            .iter()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), derived.sources.len());
    }

    #[test]
    fn empty_and_whitespace_bodies_derive_nothing() {
        assert!(derive("").is_empty());
        assert!(derive("   \n\n  \t \n").is_empty());
    }

    fn stored(conn: &Connection, body: &str) -> String {
        let memory = crate::db::models::Memory::new(
            body.to_string(),
            None,
            Some("p".into()),
            None,
            None,
            Some("project".into()),
        );
        crate::db::queries::insert_memory(conn, &memory).unwrap();
        memory.id
    }

    fn concept(conn: &Connection, id: &str) -> (Option<String>, Option<String>, i64, String) {
        conn.query_row(
            "SELECT title, description, current_revision, extensions_json
             FROM memory_concepts WHERE memory_id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    }

    #[test]
    fn a_stored_memory_is_born_enriched_without_a_second_revision() {
        let conn = crate::db::open_database(std::path::Path::new(":memory:")).unwrap();
        let id = stored(
            &conn,
            "Gateway routing: deferred constraints\n- see crates/gateway/src/db.rs\n",
        );
        let (title, description, revision, extensions) = concept(&conn, &id);
        assert_eq!(title.as_deref(), Some("Gateway routing"));
        assert_eq!(description.as_deref(), Some("deferred constraints"));
        assert_eq!(revision, 1, "derivation belongs to the insert's revision");
        assert!(extensions.contains(DERIVED_EXTENSION_KEY));
        let sources: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_sources WHERE memory_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sources, 1);
    }

    #[test]
    fn enrichment_is_idempotent_preserves_authored_values_and_keeps_verification() {
        let conn = crate::db::open_database(std::path::Path::new(":memory:")).unwrap();
        let id = stored(&conn, "Legacy body: with a claim\n");

        // Simulate a legacy row: strip what the insert derived.
        conn.execute(
            "UPDATE memory_concepts SET title = NULL, description = NULL,
             extensions_json = '{}' WHERE memory_id = ?1",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_verifications (id, memory_id, actor, verified_at, metadata_json)
             VALUES ('v1', ?1, 'human:reviewer', '2026-08-15T00:00:00Z', '{}')",
            params![id],
        )
        .unwrap();
        let before = concept(&conn, &id).2;

        let filled = enrich(&conn, &id).unwrap();
        assert_eq!(filled, vec!["title", "description"]);
        let (title, _, after_first, _) = concept(&conn, &id);
        assert_eq!(title.as_deref(), Some("Legacy body"));
        assert_eq!(after_first, before + 1, "one revision for the enrichment");

        // Derived descriptors restate the body; they do not invalidate a human
        // attestation of the claim.
        let verifications: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_verifications WHERE memory_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(verifications, 1);

        // Re-running changes nothing and mints no further revisions.
        assert!(enrich(&conn, &id).unwrap().is_empty());
        assert_eq!(concept(&conn, &id).2, after_first);

        // An authored title is never replaced.
        conn.execute(
            "UPDATE memory_concepts SET title = 'Authored' WHERE memory_id = ?1",
            params![id],
        )
        .unwrap();
        assert!(enrich(&conn, &id).unwrap().is_empty());
        assert_eq!(concept(&conn, &id).0.as_deref(), Some("Authored"));
    }

    #[test]
    fn source_keys_are_stable_per_resource() {
        assert_eq!(source_key("a/b.rs"), source_key("a/b.rs"));
        assert_ne!(source_key("a/b.rs"), source_key("a/c.rs"));
        assert!(source_key("a/b.rs").starts_with("auto-"));
    }
}
