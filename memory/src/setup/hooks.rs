//! `memory setup hooks` — automatic per-turn RAG memory injection (POC).
//!
//! Where `memory setup rules` injects a `<memory-rules>` block instructing the
//! agent to *call* `memory context` itself, this installer wires each agent
//! CLI's per-turn hook system to inject relevant memory automatically — no
//! model-initiated tool call required. The `memory hook` subcommand
//! ([`hook_command`]) does the retrieval; this module registers the per-agent
//! `memory hook --agent <agent>` command into each detected agent's config:
//!
//! | Agent  | Rule file   | Config file (resolver)                          | Event              | Timeout |
//! |--------|-------------|-------------------------------------------------|--------------------|---------|
//! | Claude | CLAUDE.md   | `settings_json::settings_path_for_rule_file`    | `UserPromptSubmit` | 10 (s)  |
//! | Gemini | GEMINI.md   | `gemini_settings_json::settings_path_for_rule_file` | `BeforeAgent`  | 10000 (ms) |
//! | Codex  | AGENTS.md   | `codex_config_toml::config_path_for_rule_file`  | `UserPromptSubmit` | (none)  |
//!
//! Agent detection reuses [`rules::detect_agent_files`] verbatim so this
//! installer and `setup rules` agree on which agents are present and where
//! their config lives (including `CODEX_HOME` / XDG precedence). The same
//! `memory hook --agent <agent>` command shape is registered for all three.
//!
//! This is opt-in: it is NOT part of `memory setup all`. The default sweep
//! still installs only gateway + rules + skill. Hooks must be requested
//! explicitly (`memory setup hooks`, or selected in the interactive menu).
//!
//! Status output uses the same light-XML `<setup status="…" .../>` vocabulary
//! as `rules.rs`, so scripted consumers grep one consistent surface.

use crate::setup::codex_config_toml;
use crate::setup::codex_hooks_toml;
use crate::setup::gemini_settings_json;
use crate::setup::hook_command::{self, HOOK_MARKER, LEGACY_SCRIPT_MARKER};
use crate::setup::json_hooks;
use crate::setup::rules;
use crate::setup::settings_json::{self, SettingsOutcome};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Claude Code hook timeout, in seconds.
const CLAUDE_TIMEOUT_SECS: i64 = 10;

/// Gemini CLI hook timeout, in milliseconds.
const GEMINI_TIMEOUT_MS: i64 = 10_000;

/// Entry point invoked from `cli.rs` for `memory setup hooks`.
///
/// Arguments:
/// - `all` — install to every detected agent without prompting. (The hooks
///   installer has no per-file interactive prompt today; this mirrors the
///   `rules::run` signature so the menu dispatch stays uniform, and is always
///   passed `true` by the current callers.)
/// - `dry_run` — print intended actions and write nothing.
/// - `print` — emit the per-agent installed commands to stdout and exit (no IO).
/// - `remove` — inverse of install: strip our hook entries (new + legacy) from
///   each agent, and best-effort delete any stale legacy bridge script.
pub fn run(all: bool, dry_run: bool, print: bool, remove: bool) -> Result<()> {
    let _ = all; // accepted for signature parity with rules::run; see doc.

    if print {
        print_installed_commands();
        return Ok(());
    }

    let candidates = rules::detect_agent_files();
    if candidates.is_empty() {
        anyhow::bail!(
            "No agent rule files detected — nothing to wire hooks into. Tried:\n  \
             ~/.claude/CLAUDE.md\n  \
             ~/.gemini/GEMINI.md\n  \
             ~/.codex/AGENTS.md\n  \
             ~/.config/codex/AGENTS.md\n\
             Install one of the agents first, then re-run `memory setup hooks`."
        );
    }

    let mut any_failed = false;

    if remove {
        for rule_file in &candidates {
            remove_for_agent(rule_file, dry_run, &mut any_failed);
        }
    } else {
        for rule_file in &candidates {
            install_for_agent(rule_file, dry_run, &mut any_failed);
        }
    }

    // Either path may leave behind a stale `memory-inject.sh` from an old
    // script-based install; clean it up so upgraded users don't keep a dead
    // file. Best-effort: a removal failure flags but does not abort.
    remove_legacy_script(dry_run, &mut any_failed);

    if any_failed {
        anyhow::bail!("one or more agents could not be updated");
    }
    Ok(())
}

// -- informational + legacy cleanup ------------------------------------------

/// Back `memory setup hooks --print`: emit the per-agent command each writer
/// would embed. Informational only — no filesystem touched.
fn print_installed_commands() {
    for agent in ["claude", "gemini", "codex"] {
        println!(
            r#"<setup status="hook_command" agent="{}" command="{}"/>"#,
            agent,
            hook_command::installed_command(agent)
        );
    }
}

/// Best-effort delete the legacy `~/.agentic/hooks/memory-inject.sh` script
/// left by pre-scriptless installs. A missing file is a silent no-op; only a
/// real deletion emits a status line. Removal errors (other than NotFound) flag
/// `any_failed` but don't abort the run.
fn remove_legacy_script(dry_run: bool, any_failed: &mut bool) {
    let Some(path) = hook_command::legacy_script_path() else {
        return;
    };
    if dry_run {
        if path.exists() {
            println!(
                r#"<setup status="legacy_script_remove_dry_run" path="{}"/>"#,
                path.display()
            );
        }
        return;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {
            println!(
                r#"<setup status="legacy_script_removed" path="{}"/>"#,
                path.display()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!(
                "Failed to remove legacy hook script {}: {e:#}",
                path.display()
            );
            *any_failed = true;
        }
    }
}

// -- per-agent dispatch ------------------------------------------------------

/// Install the per-turn hook for whichever agent `rule_file` identifies.
/// Dispatches by the rule file's name (claude.md / gemini.md / agents.md) and
/// reuses each agent's existing settings-path resolver — no path logic is
/// duplicated here.
///
/// UPGRADE-SAFE: before installing the new `memory hook --agent <agent>`
/// command we first strip any stale entry from an old script-based install
/// (matched by [`LEGACY_SCRIPT_MARKER`]). The new install's own marker-based
/// retain (keyed on [`HOOK_MARKER`]) then handles idempotency for our entries.
fn install_for_agent(rule_file: &Path, dry_run: bool, any_failed: &mut bool) {
    // Claude — settings.json, UserPromptSubmit, timeout in seconds.
    if let Some(settings_path) = settings_json::settings_path_for_rule_file(rule_file) {
        let command = hook_command::installed_command("claude");
        json_remove(
            "claude",
            &settings_path,
            "UserPromptSubmit",
            LEGACY_SCRIPT_MARKER,
            dry_run,
            any_failed,
        );
        json_install(
            "claude",
            &settings_path,
            "UserPromptSubmit",
            &command,
            CLAUDE_TIMEOUT_SECS,
            dry_run,
            any_failed,
        );
        return;
    }

    // Gemini — settings.json, BeforeAgent, timeout in milliseconds.
    if let Some(settings_path) = gemini_settings_json::settings_path_for_rule_file(rule_file) {
        let command = hook_command::installed_command("gemini");
        json_remove(
            "gemini",
            &settings_path,
            "BeforeAgent",
            LEGACY_SCRIPT_MARKER,
            dry_run,
            any_failed,
        );
        json_install(
            "gemini",
            &settings_path,
            "BeforeAgent",
            &command,
            GEMINI_TIMEOUT_MS,
            dry_run,
            any_failed,
        );
        return;
    }

    // Codex — config.toml, UserPromptSubmit, no timeout key.
    if let Some(config_path) = resolve_codex_config(rule_file) {
        let command = hook_command::installed_command("codex");
        toml_remove(
            &config_path,
            "UserPromptSubmit",
            LEGACY_SCRIPT_MARKER,
            dry_run,
            any_failed,
        );
        toml_install(
            &config_path,
            "UserPromptSubmit",
            &command,
            dry_run,
            any_failed,
        );
    }
    // Any other rule file → no hook surface to touch.
}

/// Remove the per-turn hook for whichever agent `rule_file` identifies. Strips
/// BOTH our new entries ([`HOOK_MARKER`]) and any legacy script-based entries
/// ([`LEGACY_SCRIPT_MARKER`]) so an upgrade-then-remove leaves nothing behind.
fn remove_for_agent(rule_file: &Path, dry_run: bool, any_failed: &mut bool) {
    if let Some(settings_path) = settings_json::settings_path_for_rule_file(rule_file) {
        for marker in [HOOK_MARKER, LEGACY_SCRIPT_MARKER] {
            json_remove(
                "claude",
                &settings_path,
                "UserPromptSubmit",
                marker,
                dry_run,
                any_failed,
            );
        }
        return;
    }
    if let Some(settings_path) = gemini_settings_json::settings_path_for_rule_file(rule_file) {
        for marker in [HOOK_MARKER, LEGACY_SCRIPT_MARKER] {
            json_remove(
                "gemini",
                &settings_path,
                "BeforeAgent",
                marker,
                dry_run,
                any_failed,
            );
        }
        return;
    }
    if let Some(config_path) = resolve_codex_config(rule_file) {
        for marker in [HOOK_MARKER, LEGACY_SCRIPT_MARKER] {
            toml_remove(
                &config_path,
                "UserPromptSubmit",
                marker,
                dry_run,
                any_failed,
            );
        }
    }
}

/// Resolve the Codex `config.toml` that pairs with an `AGENTS.md` rule file,
/// threading `CODEX_HOME` exactly as `rules::sync_auto_memory` does so the
/// config and rule file stay in the same Codex home.
fn resolve_codex_config(rule_file: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let codex_home_override = std::env::var("CODEX_HOME").ok().map(PathBuf::from);
    codex_config_toml::config_path_for_rule_file(rule_file, &home, codex_home_override.as_deref())
}

// -- shared executors --------------------------------------------------------

/// Install our hook command into a Claude/Gemini JSON settings file's `event`,
/// keyed on [`HOOK_MARKER`] for idempotent retain, and emit the status line.
#[allow(clippy::too_many_arguments)]
fn json_install(
    agent: &str,
    settings_path: &Path,
    event: &str,
    command: &str,
    timeout: i64,
    dry_run: bool,
    any_failed: &mut bool,
) {
    if dry_run {
        emit_dry_run(agent, settings_path, event, "install");
        return;
    }
    let outcome = json_hooks::install(settings_path, event, command, timeout, HOOK_MARKER);
    report_outcome(agent, settings_path, event, outcome, any_failed);
}

/// Remove `marker`-matching hook groups from a Claude/Gemini JSON settings
/// file's `event`, and emit the status line.
fn json_remove(
    agent: &str,
    settings_path: &Path,
    event: &str,
    marker: &str,
    dry_run: bool,
    any_failed: &mut bool,
) {
    if dry_run {
        emit_dry_run(agent, settings_path, event, "remove");
        return;
    }
    let outcome = json_hooks::remove(settings_path, event, marker);
    report_outcome(agent, settings_path, event, outcome, any_failed);
}

/// Install our hook command into a Codex `config.toml`'s `event`, keyed on
/// [`HOOK_MARKER`], and emit the status line.
fn toml_install(
    config_path: &Path,
    event: &str,
    command: &str,
    dry_run: bool,
    any_failed: &mut bool,
) {
    if dry_run {
        emit_dry_run("codex", config_path, event, "install");
        return;
    }
    let outcome = codex_hooks_toml::install(config_path, event, command, HOOK_MARKER);
    report_outcome("codex", config_path, event, outcome, any_failed);
}

/// Remove `marker`-matching hook tables from a Codex `config.toml`'s `event`,
/// and emit the status line.
fn toml_remove(
    config_path: &Path,
    event: &str,
    marker: &str,
    dry_run: bool,
    any_failed: &mut bool,
) {
    if dry_run {
        emit_dry_run("codex", config_path, event, "remove");
        return;
    }
    let outcome = codex_hooks_toml::remove(config_path, event, marker);
    report_outcome("codex", config_path, event, outcome, any_failed);
}

fn emit_dry_run(agent: &str, path: &Path, event: &str, action: &str) {
    println!(
        r#"<setup status="hooks_dry_run" agent="{}" path="{}" event="{}" action="{}"/>"#,
        agent,
        path.display(),
        event,
        action
    );
}

/// Map a `SettingsOutcome` to a hooks status line and flag failures. Install
/// outcomes render as `hooks_installed` (or `hooks_unchanged`); remove
/// outcomes as `hooks_removed` (or `hooks_already_absent`).
fn report_outcome(
    agent: &str,
    path: &Path,
    event: &str,
    outcome: Result<SettingsOutcome>,
    any_failed: &mut bool,
) {
    match outcome {
        Ok(state) => {
            let status = match state {
                SettingsOutcome::Created | SettingsOutcome::Updated => "hooks_installed",
                SettingsOutcome::AlreadyCorrect => "hooks_unchanged",
                SettingsOutcome::Removed => "hooks_removed",
                SettingsOutcome::AlreadyAbsent => "hooks_already_absent",
            };
            println!(
                r#"<setup status="{}" agent="{}" path="{}" event="{}"/>"#,
                status,
                agent,
                path.display(),
                event
            );
        }
        Err(e) => {
            eprintln!("Failed to update hooks at {}: {e:#}", path.display());
            *any_failed = true;
        }
    }
}

/// Probe helper exposed to the interactive menu: does `rule_file`'s paired
/// config already contain one of our marker-matching hook entries? Used by
/// `menu::probe_hooks` to render per-agent install state.
pub(crate) fn agent_has_hook(rule_file: &Path) -> bool {
    if let Some(settings_path) = settings_json::settings_path_for_rule_file(rule_file) {
        return json_config_has_marker(&settings_path);
    }
    if let Some(settings_path) = gemini_settings_json::settings_path_for_rule_file(rule_file) {
        return json_config_has_marker(&settings_path);
    }
    if let Some(config_path) = resolve_codex_config(rule_file) {
        return toml_config_has_marker(&config_path);
    }
    false
}

/// True when the JSON settings file at `path` mentions EITHER our new marker
/// ([`HOOK_MARKER`]) or a legacy script marker ([`LEGACY_SCRIPT_MARKER`])
/// anywhere — so the menu reports "installed" both for the new command and for
/// a not-yet-upgraded legacy entry. Best-effort: unreadable / unparseable files
/// read as "no hook" so the probe never errors the menu.
fn json_config_has_marker(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .map(|v| value_mentions_marker(&v))
        .unwrap_or(false)
}

/// True when the TOML config at `path` mentions either marker anywhere.
fn toml_config_has_marker(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .map(|raw| raw.contains(HOOK_MARKER) || raw.contains(LEGACY_SCRIPT_MARKER))
        .unwrap_or(false)
}

/// Recursively check whether any string in a JSON value contains either marker.
fn value_mentions_marker(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => s.contains(HOOK_MARKER) || s.contains(LEGACY_SCRIPT_MARKER),
        serde_json::Value::Array(a) => a.iter().any(value_mentions_marker),
        serde_json::Value::Object(o) => o.values().any(value_mentions_marker),
        _ => false,
    }
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Minimal isolated tempdir, mirroring the helper used by the sibling
    /// setup tests so this file stays dependency-free.
    fn tempdir_in_target() -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("agent-memory-hooks-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// Pull command strings out of a JSON settings file's `hooks[event]`.
    fn json_commands(path: &Path, event: &str) -> Vec<String> {
        let raw = fs::read_to_string(path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        v.get("hooks")
            .and_then(|h| h.get(event))
            .and_then(serde_json::Value::as_array)
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g.get("hooks").and_then(serde_json::Value::as_array))
                    .flatten()
                    .filter_map(|h| h.get("command").and_then(serde_json::Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Pull NESTED handler command strings out of a Codex config's
    /// `hooks[event]`: for each matcher group, collect `group.hooks[].command`.
    /// Mirrors `json_commands` — Codex hooks use the same nested matcher-group
    /// shape as Claude Code, so a flat top-level `command` read would miss them.
    fn toml_commands(path: &Path, event: &str) -> Vec<String> {
        let raw = fs::read_to_string(path).unwrap();
        let doc: toml::value::Table = toml::from_str(&raw).unwrap();
        doc.get("hooks")
            .and_then(toml::Value::as_table)
            .and_then(|h| h.get(event))
            .and_then(toml::Value::as_array)
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g.get("hooks").and_then(toml::Value::as_array))
                    .flatten()
                    .filter_map(|h| h.get("command").and_then(toml::Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Integration: stage a fake home with realistic pre-existing
    /// claude/gemini settings.json + codex config.toml — each carrying
    /// unrelated user content AND an unrelated user hook — then drive the
    /// per-agent install/remove helpers directly (mirroring how the rules.rs
    /// integration test calls `sync_auto_memory` directly rather than spawning
    /// the CLI). Asserts our hook lands in the right event without disturbing
    /// the user's content or hooks, and reverts cleanly on remove.
    #[test]
    fn install_and_remove_cycle_across_all_agents() {
        let home = tempdir_in_target();

        // -- Claude: theme + an unrelated PreToolUse hook the user added. ----
        let claude_dir = home.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let claude_rule = claude_dir.join("CLAUDE.md");
        std::fs::write(&claude_rule, "# Claude\n").unwrap();
        let claude_settings = claude_dir.join("settings.json");
        std::fs::write(
            &claude_settings,
            r#"{
  "theme": "dark",
  "hooks": {
    "PreToolUse": [
      {"matcher": "", "hooks": [{"type": "command", "command": "user-pretool.sh"}]}
    ]
  }
}
"#,
        )
        .unwrap();

        // -- Gemini: theme + an unrelated BeforeAgent hook the user added. ---
        let gemini_dir = home.join(".gemini");
        std::fs::create_dir_all(&gemini_dir).unwrap();
        let gemini_rule = gemini_dir.join("GEMINI.md");
        std::fs::write(&gemini_rule, "# Gemini\n").unwrap();
        let gemini_settings = gemini_dir.join("settings.json");
        std::fs::write(
            &gemini_settings,
            r#"{
  "theme": "dark",
  "hooks": {
    "BeforeAgent": [
      {"matcher": "", "hooks": [{"type": "command", "command": "user-beforeagent.sh"}]}
    ]
  }
}
"#,
        )
        .unwrap();

        // -- Codex: model + an unrelated hook the user added. ----------------
        let codex_dir = home.join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let codex_rule = codex_dir.join("AGENTS.md");
        std::fs::write(&codex_rule, "# Codex\n").unwrap();
        let codex_config = codex_dir.join("config.toml");
        std::fs::write(
            &codex_config,
            r#"model = "gpt-5"

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "user-codex-hook.sh"
"#,
        )
        .unwrap();

        let claude_cmd = hook_command::installed_command("claude");
        let gemini_cmd = hook_command::installed_command("gemini");
        let codex_cmd = hook_command::installed_command("codex");

        // -- install (drive helpers directly) -------------------------------
        let mut any_failed = false;
        install_for_agent(&claude_rule, false, &mut any_failed);
        install_for_agent(&gemini_rule, false, &mut any_failed);
        install_for_agent(&codex_rule, false, &mut any_failed);
        assert!(!any_failed, "install must not surface failures");

        // Claude: ours added to UserPromptSubmit, user's PreToolUse intact,
        // theme intact.
        let claude_parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude_settings).unwrap()).unwrap();
        assert_eq!(claude_parsed.get("theme").unwrap(), "dark");
        assert_eq!(
            json_commands(&claude_settings, "PreToolUse"),
            vec!["user-pretool.sh"]
        );
        assert_eq!(
            json_commands(&claude_settings, "UserPromptSubmit"),
            vec![claude_cmd.clone()]
        );

        // Gemini: ours appended to BeforeAgent alongside the user's hook.
        let gemini_before = json_commands(&gemini_settings, "BeforeAgent");
        assert!(gemini_before.contains(&"user-beforeagent.sh".to_string()));
        assert!(gemini_before.contains(&gemini_cmd));

        // Codex: ours appended to UserPromptSubmit alongside the user's hook,
        // model intact.
        let codex_cmds = toml_commands(&codex_config, "UserPromptSubmit");
        assert!(codex_cmds.contains(&"user-codex-hook.sh".to_string()));
        assert!(codex_cmds.contains(&codex_cmd));
        let codex_parsed: toml::value::Table =
            toml::from_str(&fs::read_to_string(&codex_config).unwrap()).unwrap();
        assert_eq!(codex_parsed.get("model").unwrap().as_str(), Some("gpt-5"));

        // -- remove ---------------------------------------------------------
        let mut any_failed = false;
        remove_for_agent(&claude_rule, false, &mut any_failed);
        remove_for_agent(&gemini_rule, false, &mut any_failed);
        remove_for_agent(&codex_rule, false, &mut any_failed);
        assert!(!any_failed, "remove must not surface failures");

        // Claude reverts: ours gone, user's PreToolUse + theme intact.
        let claude_parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&claude_settings).unwrap()).unwrap();
        assert_eq!(claude_parsed.get("theme").unwrap(), "dark");
        assert_eq!(
            json_commands(&claude_settings, "PreToolUse"),
            vec!["user-pretool.sh"]
        );
        assert!(
            json_commands(&claude_settings, "UserPromptSubmit").is_empty(),
            "our UserPromptSubmit group must be gone"
        );

        // Gemini reverts: ours gone, user's BeforeAgent hook intact.
        assert_eq!(
            json_commands(&gemini_settings, "BeforeAgent"),
            vec!["user-beforeagent.sh"]
        );

        // Codex reverts: ours gone, user's hook + model intact.
        assert_eq!(
            toml_commands(&codex_config, "UserPromptSubmit"),
            vec!["user-codex-hook.sh"]
        );
        let codex_parsed: toml::value::Table =
            toml::from_str(&fs::read_to_string(&codex_config).unwrap()).unwrap();
        assert_eq!(codex_parsed.get("model").unwrap().as_str(), Some("gpt-5"));
    }

    #[test]
    fn agent_has_hook_detects_installed_entry() {
        let home = tempdir_in_target();
        let claude_dir = home.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let claude_rule = claude_dir.join("CLAUDE.md");
        std::fs::write(&claude_rule, "# Claude\n").unwrap();

        // Before install: no hook.
        assert!(!agent_has_hook(&claude_rule));

        let mut any_failed = false;
        install_for_agent(&claude_rule, false, &mut any_failed);
        assert!(!any_failed);

        // After install: detected.
        assert!(agent_has_hook(&claude_rule));
    }

    /// Upgrade path: a pre-existing LEGACY script-based entry (command contains
    /// `memory-inject`) must be stripped on install and replaced by the new
    /// binary command. Exercised on Codex, whose seed carries a legacy nested
    /// handler plus an unrelated user hook + model that must survive.
    #[test]
    fn install_strips_legacy_script_entry() {
        let home = tempdir_in_target();
        let codex_dir = home.join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let codex_rule = codex_dir.join("AGENTS.md");
        std::fs::write(&codex_rule, "# Codex\n").unwrap();
        let codex_config = codex_dir.join("config.toml");
        // Seed: model + an unrelated user hook + a LEGACY memory-inject entry.
        std::fs::write(
            &codex_config,
            r#"model = "gpt-5"

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "user-codex-hook.sh"

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = "/home/u/.agentic/hooks/memory-inject.sh codex"
"#,
        )
        .unwrap();

        let codex_cmd = hook_command::installed_command("codex");

        let mut any_failed = false;
        install_for_agent(&codex_rule, false, &mut any_failed);
        assert!(!any_failed, "install must not surface failures");

        let cmds = toml_commands(&codex_config, "UserPromptSubmit");
        // Legacy entry gone, new binary command present, user's hook intact.
        assert!(
            !cmds.iter().any(|c| c.contains(LEGACY_SCRIPT_MARKER)),
            "legacy memory-inject entry must be stripped, got {cmds:?}"
        );
        assert!(cmds.contains(&codex_cmd), "new command missing: {cmds:?}");
        assert!(
            cmds.contains(&"user-codex-hook.sh".to_string()),
            "user's hook must survive: {cmds:?}"
        );
        let parsed: toml::value::Table =
            toml::from_str(&fs::read_to_string(&codex_config).unwrap()).unwrap();
        assert_eq!(parsed.get("model").unwrap().as_str(), Some("gpt-5"));
    }
}
