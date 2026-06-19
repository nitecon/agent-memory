//! Shared `settings.json` hook merge used by `memory setup hooks` for both
//! Claude Code and Gemini CLI.
//!
//! Both frontends store per-turn hooks under the same JSON shape, so a single
//! merge helper serves both — only the event name and timeout differ at the
//! call site:
//!
//! ```json
//! {
//!   "hooks": {
//!     "<EVENT>": [
//!       {
//!         "matcher": "",
//!         "hooks": [
//!           { "type": "command", "command": "…/memory-inject.sh <agent>", "timeout": N }
//!         ]
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! Claude Code wires the bridge into `UserPromptSubmit` (timeout in seconds);
//! Gemini CLI wires it into `BeforeAgent` (timeout in milliseconds). The
//! merge logic is identical; the caller supplies the event, command, timeout,
//! and the marker (see [`crate::setup::hook_command::HOOK_MARKER`]) used to
//! find our own entries.
//!
//! The merge is deliberately conservative, matching the sibling per-agent
//! settings helpers:
//!
//! - Parse the existing file as a JSON object and preserve every key the user
//!   has (`theme`, `model`, permissions, OTHER hook events, OTHER groups in
//!   the same event the user added by hand).
//! - Fail loudly on corrupt / non-object JSON, on a present-but-non-object
//!   `hooks`, and on a present-but-non-array `hooks[event]` rather than
//!   silently overwriting a shape we don't understand.
//! - Writes are atomic (`.new` + rename).
//! - Re-running is idempotent: stale copies of OUR group (matched by marker)
//!   are removed first, then exactly one fresh group is pushed. When the
//!   result is byte-identical to what was already there we report
//!   `AlreadyCorrect` and skip the write entirely — no mtime churn.
//!
//! On remove, our marker-matching groups are dropped from `hooks[event]`. If
//! that array empties we drop the event key; if `hooks` empties we drop the
//! `hooks` key. The file is never deleted — consistent with every other
//! remove flow in this module, we write `{}` if the object empties.

use crate::setup::settings_json::SettingsOutcome;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Install (or refresh) our per-turn hook group in `path` under `event`.
///
/// Semantics:
///
/// 1. Missing file → create with just the `hooks[event]` group.
/// 2. Existing object → ensure `hooks` is an object and `hooks[event]` is an
///    array, drop any prior group of ours (matched by `marker`), then append
///    a fresh group. Every other key, event, and user-authored group is
///    preserved.
/// 3. Corrupt / non-object top level, non-object `hooks`, or non-array
///    `hooks[event]` → bail with a typed error naming the file.
/// 4. Result identical to the pre-existing content → no-op (`AlreadyCorrect`).
pub fn install(
    path: &Path,
    event: &str,
    command: &str,
    timeout: i64,
    marker: &str,
) -> Result<SettingsOutcome> {
    let fresh_group = group(command, timeout);

    match read_object(path)? {
        None => {
            let mut obj = Map::new();
            let mut hooks = Map::new();
            hooks.insert(event.to_string(), Value::Array(vec![fresh_group]));
            obj.insert("hooks".to_string(), Value::Object(hooks));
            write_object(path, &obj)?;
            Ok(SettingsOutcome::Created)
        }
        Some(mut obj) => {
            // Snapshot for the idempotency check below.
            let before = serde_json::to_string(&obj).ok();

            let hooks = ensure_hooks_object(&mut obj, path)?;
            let arr = ensure_event_array(hooks, event, path)?;

            // Drop any stale copy of our own group, then push a fresh one.
            arr.retain(|g| !group_has_marker(g, marker));
            arr.push(fresh_group);

            let after = serde_json::to_string(&obj).ok();
            if before.is_some() && before == after {
                return Ok(SettingsOutcome::AlreadyCorrect);
            }
            write_object(path, &obj)?;
            Ok(SettingsOutcome::Updated)
        }
    }
}

/// Remove our marker-matching hook groups from `hooks[event]` in `path`.
///
/// - Missing file → `AlreadyAbsent`.
/// - No `hooks` / no `hooks[event]` / no marker-matching group → `AlreadyAbsent`.
/// - Removed at least one → collapse an empty `hooks[event]` (drop the event
///   key), then an empty `hooks` (drop the key), then write back. The file is
///   never deleted; an emptied object is written as `{}`.
pub fn remove(path: &Path, event: &str, marker: &str) -> Result<SettingsOutcome> {
    match read_object(path)? {
        None => Ok(SettingsOutcome::AlreadyAbsent),
        Some(mut obj) => {
            let Some(hooks_val) = obj.get_mut("hooks") else {
                return Ok(SettingsOutcome::AlreadyAbsent);
            };
            let hooks = match hooks_val {
                Value::Object(m) => m,
                other => bail!(
                    "settings file {} has wrong shape for `hooks`: expected object, got {}",
                    path.display(),
                    shape_name(other)
                ),
            };
            let Some(event_val) = hooks.get_mut(event) else {
                return Ok(SettingsOutcome::AlreadyAbsent);
            };
            let arr = match event_val {
                Value::Array(a) => a,
                other => bail!(
                    "settings file {} has wrong shape for `hooks.{}`: expected array, got {}",
                    path.display(),
                    event,
                    shape_name(other)
                ),
            };
            let original_len = arr.len();
            arr.retain(|g| !group_has_marker(g, marker));
            if arr.len() == original_len {
                return Ok(SettingsOutcome::AlreadyAbsent);
            }
            // Collapse empties from the inside out so we don't leave dangling
            // `"<event>": []` or `"hooks": {}` shrapnel behind.
            if arr.is_empty() {
                hooks.remove(event);
            }
            if hooks.is_empty() {
                obj.remove("hooks");
            }
            write_object(path, &obj)?;
            Ok(SettingsOutcome::Removed)
        }
    }
}

// -- shape helpers -----------------------------------------------------------

/// Build a fresh hook group: a `matcher`-keyed wrapper around a single
/// `command` hook. The empty matcher means "every turn", which is what we
/// want for unconditional per-turn injection.
fn group(command: &str, timeout: i64) -> Value {
    json!({
        "matcher": "",
        "hooks": [
            { "type": "command", "command": command, "timeout": timeout }
        ]
    })
}

/// True when `group` contains a command hook whose `command` string contains
/// `marker` — i.e. it is one we installed. Tolerant of arbitrary nesting the
/// user might have under `hooks`, but only matches on the documented
/// `hooks[].command` location.
fn group_has_marker(group: &Value, marker: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|inner| {
            inner.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(marker))
            })
        })
        .unwrap_or(false)
}

/// Ensure top-level `hooks` is an object and return a mutable ref. Creates it
/// when missing; bails when present but the wrong shape.
fn ensure_hooks_object<'a>(
    obj: &'a mut Map<String, Value>,
    path: &Path,
) -> Result<&'a mut Map<String, Value>> {
    let entry = obj
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    match entry {
        Value::Object(m) => Ok(m),
        other => bail!(
            "settings file {} has wrong shape for `hooks`: expected object, got {}",
            path.display(),
            shape_name(other)
        ),
    }
}

/// Ensure `hooks[event]` is an array and return a mutable ref. Creates it when
/// missing; bails when present but the wrong shape.
fn ensure_event_array<'a>(
    hooks: &'a mut Map<String, Value>,
    event: &str,
    path: &Path,
) -> Result<&'a mut Vec<Value>> {
    let entry = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    match entry {
        Value::Array(a) => Ok(a),
        other => bail!(
            "settings file {} has wrong shape for `hooks.{}`: expected array, got {}",
            path.display(),
            event,
            shape_name(other)
        ),
    }
}

// -- I/O helpers (local copies so this module stays self-contained) ----------

/// Read `path` and parse it as a JSON object. Mirrors the sibling helpers in
/// `settings_json` / `gemini_settings_json` but kept local so each module is
/// independently auditable.
fn read_object(path: &Path) -> Result<Option<Map<String, Value>>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!(e).context(format!("read settings file {}", path.display()))),
    };

    if raw.trim().is_empty() {
        return Ok(None);
    }

    let value: Value = serde_json::from_str(&raw).with_context(|| {
        format!(
            "parse settings file {} as JSON (is it JSONC? comments are not supported)",
            path.display()
        )
    })?;

    match value {
        Value::Object(map) => Ok(Some(map)),
        other => bail!(
            "settings file {} has unexpected top-level shape: expected object, got {}",
            path.display(),
            shape_name(&other)
        ),
    }
}

/// Write a JSON object atomically with sorted top-level keys and 2-space
/// indent. Same deterministic-output discipline as the sibling helpers.
fn write_object(path: &Path, obj: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent of {}", path.display()))?;
    }

    let sorted: std::collections::BTreeMap<&String, &Value> = obj.iter().collect();
    let mut body = serde_json::to_string_pretty(&sorted)
        .with_context(|| format!("serialize settings for {}", path.display()))?;
    body.push('\n');

    let tmp = temp_path(path);
    std::fs::write(&tmp, &body)
        .with_context(|| format!("write temp settings file {}", tmp.display()))?;
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
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const MARKER: &str = "memory-inject";
    const CMD: &str = "/home/u/.agentic/hooks/memory-inject.sh claude";

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "agent-memory-json-hooks-test-{}",
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

    /// Pull the command strings out of every group under `hooks[event]`.
    fn commands(obj: &Value, event: &str) -> Vec<String> {
        obj.get("hooks")
            .and_then(|h| h.get(event))
            .and_then(Value::as_array)
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g.get("hooks").and_then(Value::as_array))
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
        let path = dir.path().join("settings.json");
        let out = install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Created);
        let parsed: Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(commands(&parsed, "UserPromptSubmit"), vec![CMD.to_string()]);
        assert!(read(&path).ends_with('\n'));
    }

    #[test]
    fn install_preserves_unrelated_keys_events_and_user_hooks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "theme": "dark",
  "hooks": {
    "PreToolUse": [
      {"matcher": "", "hooks": [{"type": "command", "command": "user-tool.sh"}]}
    ],
    "UserPromptSubmit": [
      {"matcher": "", "hooks": [{"type": "command", "command": "user-prompt.sh"}]}
    ]
  }
}
"#,
        )
        .unwrap();

        let out = install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Updated);

        let parsed: Value = serde_json::from_str(&read(&path)).unwrap();
        let obj = parsed.as_object().unwrap();
        // Unrelated top-level key preserved.
        assert_eq!(obj.get("theme").unwrap(), "dark");
        // Unrelated event preserved.
        assert_eq!(commands(&parsed, "PreToolUse"), vec!["user-tool.sh"]);
        // Our event now holds BOTH the user's hook and ours.
        let cmds = commands(&parsed, "UserPromptSubmit");
        assert!(cmds.contains(&"user-prompt.sh".to_string()), "got {cmds:?}");
        assert!(cmds.contains(&CMD.to_string()), "got {cmds:?}");
    }

    #[test]
    fn install_is_idempotent_no_duplicate_and_no_mtime_churn() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap();

        let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let out = install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::AlreadyCorrect);
        let after_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            before_mtime, after_mtime,
            "idempotent re-run must not rewrite the file"
        );

        // Exactly one of our groups, never accumulating.
        let parsed: Value = serde_json::from_str(&read(&path)).unwrap();
        let ours = commands(&parsed, "UserPromptSubmit")
            .into_iter()
            .filter(|c| c.contains(MARKER))
            .count();
        assert_eq!(ours, 1);
    }

    #[test]
    fn install_replaces_stale_copy_of_our_group() {
        // Simulate a prior install that used a different timeout. The refresh
        // must drop the stale group and leave exactly one.
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        install(&path, "UserPromptSubmit", CMD, 5, MARKER).unwrap();
        let out = install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Updated);

        let parsed: Value = serde_json::from_str(&read(&path)).unwrap();
        let ours = commands(&parsed, "UserPromptSubmit")
            .into_iter()
            .filter(|c| c.contains(MARKER))
            .count();
        assert_eq!(ours, 1, "stale copy must be replaced, not appended");
        // New timeout landed.
        let groups = parsed
            .get("hooks")
            .and_then(|h| h.get("UserPromptSubmit"))
            .and_then(Value::as_array)
            .unwrap();
        let timeout = groups
            .iter()
            .filter_map(|g| g.get("hooks").and_then(Value::as_array))
            .flatten()
            .find(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(MARKER))
            })
            .and_then(|h| h.get("timeout"))
            .and_then(Value::as_i64);
        assert_eq!(timeout, Some(10));
    }

    #[test]
    fn install_bails_on_corrupt_json_without_mutating() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = r#"{"theme": "dark",,,"#;
        fs::write(&path, original).unwrap();
        let err = install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap_err();
        assert!(format!("{err:#}").contains("settings.json"));
        assert_eq!(read(&path), original, "corrupt file was modified");
    }

    #[test]
    fn install_bails_on_non_object_hooks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = r#"{"hooks": [1, 2, 3]}"#;
        fs::write(&path, original).unwrap();
        let err = install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap_err();
        assert!(format!("{err:#}").contains("expected object"));
        assert_eq!(read(&path), original);
    }

    #[test]
    fn install_bails_on_non_array_event() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = r#"{"hooks": {"UserPromptSubmit": "nope"}}"#;
        fs::write(&path, original).unwrap();
        let err = install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap_err();
        assert!(format!("{err:#}").contains("expected array"));
        assert_eq!(read(&path), original);
    }

    #[test]
    fn remove_strips_only_our_group_and_collapses_empties() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap();

        let out = remove(&path, "UserPromptSubmit", MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Removed);
        // hooks key dropped entirely once the only group was ours.
        let parsed: Value = serde_json::from_str(&read(&path)).unwrap();
        assert!(parsed.as_object().unwrap().get("hooks").is_none());
    }

    #[test]
    fn remove_preserves_user_group_in_same_event() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "hooks": {
    "UserPromptSubmit": [
      {"matcher": "", "hooks": [{"type": "command", "command": "user-prompt.sh"}]}
    ]
  }
}
"#,
        )
        .unwrap();
        install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap();

        let out = remove(&path, "UserPromptSubmit", MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Removed);
        let parsed: Value = serde_json::from_str(&read(&path)).unwrap();
        let cmds = commands(&parsed, "UserPromptSubmit");
        assert_eq!(cmds, vec!["user-prompt.sh"], "user hook must survive");
    }

    #[test]
    fn remove_noop_when_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"theme": "dark"}"#).unwrap();
        let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let out = remove(&path, "UserPromptSubmit", MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::AlreadyAbsent);
        let after_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before_mtime, after_mtime);
    }

    #[test]
    fn remove_noop_when_file_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let out = remove(&path, "UserPromptSubmit", MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::AlreadyAbsent);
        assert!(!path.exists());
    }

    #[test]
    fn install_then_remove_round_trips_to_original() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // Start with a user object that survives the cycle.
        fs::write(&path, "{\n  \"theme\": \"dark\"\n}\n").unwrap();
        install(&path, "UserPromptSubmit", CMD, 10, MARKER).unwrap();
        remove(&path, "UserPromptSubmit", MARKER).unwrap();
        let parsed: Value = serde_json::from_str(&read(&path)).unwrap();
        let obj = parsed.as_object().unwrap();
        assert_eq!(obj.get("theme").unwrap(), "dark");
        assert!(obj.get("hooks").is_none());
    }

    /// Gemini uses the same shape under a different event/timeout — verify the
    /// helper is genuinely agent-agnostic.
    #[test]
    fn install_works_for_gemini_before_agent_event() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let gemini_cmd = "/home/u/.agentic/hooks/memory-inject.sh gemini";
        let out = install(&path, "BeforeAgent", gemini_cmd, 10000, MARKER).unwrap();
        assert_eq!(out, SettingsOutcome::Created);
        let parsed: Value = serde_json::from_str(&read(&path)).unwrap();
        assert_eq!(
            commands(&parsed, "BeforeAgent"),
            vec![gemini_cmd.to_string()]
        );
    }
}
