//! Codex `config.toml` hook merge used by `memory setup hooks`.
//!
//! Codex CLI 0.141 expresses per-turn hooks with a NESTED, Claude-Code-compatible
//! shape: `hooks.<event>` is an array of *matcher groups*, and each group carries
//! its own inner `hooks` array of handler tables:
//!
//! ```toml
//! [[hooks.UserPromptSubmit]]
//!
//! [[hooks.UserPromptSubmit.hooks]]
//! type = "command"
//! command = "/abs/path/memory-inject.sh codex"
//! ```
//!
//! Structurally:
//! `hooks.UserPromptSubmit = [ { hooks = [ { type = "command", command = "…" } ] } ]`.
//! The matcher group's top-level `matcher` key is optional and is omitted — Codex
//! accepts an empty matcher group. A flat `{ type, command }` table at the group
//! level has no inner `hooks` handler array, so Codex finds zero handlers and
//! `/hooks` reports Installed 0 / Active 0; the nested shape is required.
//!
//! `memory setup hooks` adds one such matcher group (with a single nested handler)
//! to `hooks.UserPromptSubmit` so the bridge script runs on every Codex turn. No
//! `timeout` key is written: the unit Codex expects there is uncertain for this
//! POC, and the default behavior is fine, so we omit it rather than guess.
//!
//! The merge discipline matches the sibling `codex_config_toml` helper that
//! disables native memory:
//!
//! - Parse the existing file as a TOML document, preserving every other
//!   top-level table (`model`, `sandbox_mode`, `[features]`, `[projects.*]`,
//!   `[tui]`, …) and every other hook event.
//! - Fail loudly on corrupt TOML rather than silently overwriting.
//! - Writes are atomic (`.new` + rename).
//! - Idempotent: stale copies of OUR table (matched by marker) are dropped
//!   first, then exactly one fresh table is pushed. A byte-identical result
//!   skips the write (`AlreadyCorrect`).
//!
//! On remove, our marker-matching tables are dropped from `hooks.<event>`. An
//! emptied event array is removed, then an emptied `hooks` table is removed.
//! The file is never deleted — same least-surprise policy as every other
//! remove flow here.

use crate::setup::settings_json::SettingsOutcome;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use toml::Value;

/// TOML table that groups all Codex hook events.
pub const HOOKS_TABLE: &str = "hooks";

/// Install (or refresh) our per-turn hook table in `config_path` under
/// `event`.
///
/// 1. Missing file → create with just `hooks.<event>` holding our table.
/// 2. Existing document → ensure `hooks` is a table and `hooks[event]` is an
///    array, drop any prior table of ours (matched by `marker`), then push a
///    fresh one. Every other table and event is preserved.
/// 3. Corrupt TOML, non-table `hooks`, or non-array `hooks[event]` → bail.
/// 4. Result identical to pre-existing content → no-op (`AlreadyCorrect`).
pub fn install(
    config_path: &Path,
    event: &str,
    command: &str,
    marker: &str,
) -> Result<SettingsOutcome> {
    let fresh = table(command);

    match read_document(config_path)? {
        None => {
            let mut doc = toml::value::Table::new();
            let mut hooks = toml::value::Table::new();
            hooks.insert(event.to_string(), Value::Array(vec![fresh]));
            doc.insert(HOOKS_TABLE.to_string(), Value::Table(hooks));
            write_document(config_path, &doc)?;
            Ok(SettingsOutcome::Created)
        }
        Some(mut doc) => {
            let before = toml::to_string(&doc).ok();

            let hooks = ensure_hooks_table(&mut doc, config_path)?;
            let arr = ensure_event_array(hooks, event, config_path)?;

            arr.retain(|t| !table_has_marker(t, marker));
            arr.push(fresh);

            let after = toml::to_string(&doc).ok();
            if before.is_some() && before == after {
                return Ok(SettingsOutcome::AlreadyCorrect);
            }
            write_document(config_path, &doc)?;
            Ok(SettingsOutcome::Updated)
        }
    }
}

/// Remove our marker-matching hook tables from `hooks.<event>` in
/// `config_path`.
///
/// - Missing file → `AlreadyAbsent`.
/// - No `hooks` / no `hooks[event]` / no marker-matching table → `AlreadyAbsent`.
/// - Removed at least one → collapse an empty `hooks[event]` then an empty
///   `hooks` table, write back. The file is never deleted.
pub fn remove(config_path: &Path, event: &str, marker: &str) -> Result<SettingsOutcome> {
    match read_document(config_path)? {
        None => Ok(SettingsOutcome::AlreadyAbsent),
        Some(mut doc) => {
            let Some(hooks_val) = doc.get_mut(HOOKS_TABLE) else {
                return Ok(SettingsOutcome::AlreadyAbsent);
            };
            let hooks = match hooks_val {
                Value::Table(t) => t,
                other => bail!(
                    "config file {} has wrong shape for `[{}]`: expected table, got {}",
                    config_path.display(),
                    HOOKS_TABLE,
                    shape_name(other)
                ),
            };
            let Some(event_val) = hooks.get_mut(event) else {
                return Ok(SettingsOutcome::AlreadyAbsent);
            };
            let arr = match event_val {
                Value::Array(a) => a,
                other => bail!(
                    "config file {} has wrong shape for `[[{}.{}]]`: expected array, got {}",
                    config_path.display(),
                    HOOKS_TABLE,
                    event,
                    shape_name(other)
                ),
            };
            let original_len = arr.len();
            arr.retain(|t| !table_has_marker(t, marker));
            if arr.len() == original_len {
                return Ok(SettingsOutcome::AlreadyAbsent);
            }
            if arr.is_empty() {
                hooks.remove(event);
            }
            if hooks.is_empty() {
                doc.remove(HOOKS_TABLE);
            }
            write_document(config_path, &doc)?;
            Ok(SettingsOutcome::Removed)
        }
    }
}

// -- shape helpers -----------------------------------------------------------

/// Build a fresh Codex matcher group:
/// `{ hooks = [ { type = "command", command = "…" } ] }`. No `matcher` key (Codex
/// accepts an empty group) and no `timeout` key (unit uncertain for this POC, so
/// omitted).
fn table(command: &str) -> Value {
    let mut handler = toml::value::Table::new();
    handler.insert("type".to_string(), Value::String("command".to_string()));
    handler.insert("command".to_string(), Value::String(command.to_string()));

    let mut group = toml::value::Table::new();
    group.insert(
        HOOKS_TABLE.to_string(),
        Value::Array(vec![Value::Table(handler)]),
    );
    Value::Table(group)
}

/// True when any handler inside the matcher group's nested `hooks` array has a
/// `command` string containing `marker` — i.e. the group is one we installed.
fn table_has_marker(t: &Value, marker: &str) -> bool {
    t.get(HOOKS_TABLE)
        .and_then(Value::as_array)
        .is_some_and(|handlers| {
            handlers.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(marker))
            })
        })
}

/// Ensure `[hooks]` exists as a table and return a mutable ref. Creates it
/// when missing; bails when present but the wrong shape.
fn ensure_hooks_table<'a>(
    doc: &'a mut toml::value::Table,
    path: &Path,
) -> Result<&'a mut toml::value::Table> {
    let entry = doc
        .entry(HOOKS_TABLE.to_string())
        .or_insert_with(|| Value::Table(toml::value::Table::new()));
    match entry {
        Value::Table(t) => Ok(t),
        other => bail!(
            "config file {} has wrong shape for `[{}]`: expected table, got {}",
            path.display(),
            HOOKS_TABLE,
            shape_name(other)
        ),
    }
}

/// Ensure `hooks[event]` is an array-of-tables and return a mutable ref.
/// Creates it when missing; bails when present but the wrong shape.
fn ensure_event_array<'a>(
    hooks: &'a mut toml::value::Table,
    event: &str,
    path: &Path,
) -> Result<&'a mut Vec<Value>> {
    let entry = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    match entry {
        Value::Array(a) => Ok(a),
        other => bail!(
            "config file {} has wrong shape for `[[{}.{}]]`: expected array, got {}",
            path.display(),
            HOOKS_TABLE,
            event,
            shape_name(other)
        ),
    }
}

// -- I/O helpers (local copies, mirroring codex_config_toml) -----------------

/// Read `path` and parse it as a TOML document. Returns `Ok(None)` when the
/// file is missing or empty. Parse failures surface the file path in context.
fn read_document(path: &Path) -> Result<Option<toml::value::Table>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("read config file {}", path.display()))
            )
        }
    };

    if raw.trim().is_empty() {
        return Ok(None);
    }

    let doc: toml::value::Table = toml::from_str(&raw)
        .with_context(|| format!("parse config file {} as TOML", path.display()))?;
    Ok(Some(doc))
}

/// Serialize a TOML table and write atomically. `toml::to_string_pretty`
/// renders array-of-tables as `[[hooks.<event>]]`, matching a hand-edited
/// Codex config.
fn write_document(path: &Path, doc: &toml::value::Table) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent of {}", path.display()))?;
    }

    let body = toml::to_string_pretty(doc)
        .with_context(|| format!("serialize config for {}", path.display()))?;

    let tmp = temp_path(path);
    std::fs::write(&tmp, &body)
        .with_context(|| format!("write temp config file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("atomically rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".new");
    PathBuf::from(s)
}

fn shape_name(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "string",
        Value::Integer(_) => "integer",
        Value::Float(_) => "float",
        Value::Boolean(_) => "bool",
        Value::Datetime(_) => "datetime",
        Value::Array(_) => "array",
        Value::Table(_) => "table",
    }
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const MARKER: &str = "memory-inject";
    const CMD: &str = "/home/u/.agentic/hooks/memory-inject.sh codex";
    const EVENT: &str = "UserPromptSubmit";

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "agent-memory-codex-hooks-test-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn tempdir() -> std::io::Result<TestDir> {
        Ok(TestDir::new())
    }

    /// Pull command strings out of the nested handlers under `hooks[event]`:
    /// for each matcher group, collect `group["hooks"][].command`.
    fn commands(doc: &toml::value::Table, event: &str) -> Vec<String> {
        doc.get(HOOKS_TABLE)
            .and_then(Value::as_table)
            .and_then(|h| h.get(event))
            .and_then(Value::as_array)
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g.get(HOOKS_TABLE).and_then(Value::as_array))
                    .flatten()
                    .filter_map(|h| h.get("command").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn install_creates_file_when_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = install(&path, EVENT, CMD, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Created);
        let body = read(&path);
        assert!(body.contains("[[hooks.UserPromptSubmit]]"), "got: {body}");
        assert!(
            body.contains("[[hooks.UserPromptSubmit.hooks]]"),
            "got: {body}"
        );
        assert!(body.contains(CMD), "got: {body}");
        // No timeout key per the POC contract.
        assert!(!body.contains("timeout"), "got: {body}");
    }

    #[test]
    fn install_preserves_realistic_config() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"model = "gpt-5"
sandbox_mode = "workspace-write"

[features]
memories = false

[projects."/home/u/work"]
trust_level = "trusted"

[tui]
theme = "dark"
"#,
        )
        .unwrap();

        let out = install(&path, EVENT, CMD, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Updated);

        let parsed: toml::value::Table = toml::from_str(&read(&path)).unwrap();
        // Top-level scalars preserved.
        assert_eq!(parsed.get("model").unwrap().as_str(), Some("gpt-5"));
        assert_eq!(
            parsed.get("sandbox_mode").unwrap().as_str(),
            Some("workspace-write")
        );
        // [features] preserved.
        assert_eq!(
            parsed
                .get("features")
                .and_then(Value::as_table)
                .and_then(|t| t.get("memories"))
                .and_then(Value::as_bool),
            Some(false)
        );
        // [projects."…"] preserved.
        let projects = parsed.get("projects").and_then(Value::as_table).unwrap();
        assert!(projects.contains_key("/home/u/work"));
        // [tui] preserved.
        assert_eq!(
            parsed
                .get("tui")
                .and_then(Value::as_table)
                .and_then(|t| t.get("theme"))
                .and_then(Value::as_str),
            Some("dark")
        );
        // Our hook landed.
        assert_eq!(commands(&parsed, EVENT), vec![CMD.to_string()]);
    }

    #[test]
    fn install_preserves_user_hook_in_same_event() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "user-hook.sh"
"#,
        )
        .unwrap();

        install(&path, EVENT, CMD, MARKER).unwrap();
        let parsed: toml::value::Table = toml::from_str(&read(&path)).unwrap();
        let cmds = commands(&parsed, EVENT);
        assert!(cmds.contains(&"user-hook.sh".to_string()), "got {cmds:?}");
        assert!(cmds.contains(&CMD.to_string()), "got {cmds:?}");
    }

    #[test]
    fn install_is_idempotent_no_duplicate_no_mtime_churn() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        install(&path, EVENT, CMD, MARKER).unwrap();

        let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let out = install(&path, EVENT, CMD, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::AlreadyCorrect);
        let after_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before_mtime, after_mtime);

        let parsed: toml::value::Table = toml::from_str(&read(&path)).unwrap();
        let ours = commands(&parsed, EVENT)
            .into_iter()
            .filter(|c| c.contains(MARKER))
            .count();
        assert_eq!(ours, 1);
    }

    #[test]
    fn install_bails_on_corrupt_toml_without_mutating() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[hooks\nbroken = true\n";
        fs::write(&path, original).unwrap();
        let err = install(&path, EVENT, CMD, MARKER).unwrap_err();
        assert!(format!("{err:#}").contains("config.toml"));
        assert_eq!(read(&path), original, "corrupt file was modified");
    }

    #[test]
    fn install_bails_on_non_table_hooks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "hooks = \"nope\"\n";
        fs::write(&path, original).unwrap();
        let err = install(&path, EVENT, CMD, MARKER).unwrap_err();
        assert!(format!("{err:#}").contains("expected table"));
        assert_eq!(read(&path), original);
    }

    #[test]
    fn remove_strips_our_table_and_collapses_empties() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "model = \"gpt-5\"\n").unwrap();
        install(&path, EVENT, CMD, MARKER).unwrap();

        let out = remove(&path, EVENT, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Removed);
        let parsed: toml::value::Table = toml::from_str(&read(&path)).unwrap();
        assert!(
            !parsed.contains_key(HOOKS_TABLE),
            "empty hooks table must be dropped"
        );
        // Unrelated content survives the cycle.
        assert_eq!(parsed.get("model").unwrap().as_str(), Some("gpt-5"));
    }

    #[test]
    fn remove_preserves_user_hook_in_same_event() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "user-hook.sh"
"#,
        )
        .unwrap();
        install(&path, EVENT, CMD, MARKER).unwrap();

        let out = remove(&path, EVENT, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Removed);
        let parsed: toml::value::Table = toml::from_str(&read(&path)).unwrap();
        assert_eq!(commands(&parsed, EVENT), vec!["user-hook.sh"]);
    }

    #[test]
    fn remove_noop_when_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "model = \"gpt-5\"\n").unwrap();
        let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let out = remove(&path, EVENT, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::AlreadyAbsent);
        let after_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before_mtime, after_mtime);
    }

    #[test]
    fn remove_noop_when_file_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let out = remove(&path, EVENT, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::AlreadyAbsent);
        assert!(!path.exists());
    }
}
