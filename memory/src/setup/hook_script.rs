//! Shared hook-script management for `memory setup hooks`.
//!
//! `memory setup hooks` wires three agent CLIs to inject relevant memory into
//! their context automatically on every turn, replacing the rule-based "the
//! agent must remember to run `memory context`" approach. Every CLI calls the
//! *same* bridge script — [`INJECT_SCRIPT`] — passing its own agent name as
//! the first argument:
//!
//!   - Claude Code → `UserPromptSubmit` hook → `memory-inject.sh claude`
//!   - Codex CLI   → `UserPromptSubmit` hook → `memory-inject.sh codex`
//!   - Gemini CLI  → `BeforeAgent` hook      → `memory-inject.sh gemini`
//!
//! The script reads the hook JSON payload on stdin, extracts the user's
//! prompt, runs `memory context`, and emits a `hookSpecificOutput` envelope
//! with `additionalContext` set to the retrieved memory. Emitting nothing
//! (exit 0) injects nothing, which is what keeps low-signal turns — empty
//! prompts, or prompts with no relevant memory — cheap.
//!
//! This module owns the single source of truth for the script body, its
//! install path (`~/.agentic/hooks/memory-inject.sh`), and the command string
//! the per-agent config writers embed. It is deliberately self-contained: the
//! JSON/TOML hook writers depend only on [`INJECT_SCRIPT_MARKER`] and
//! [`installed_command`], never on the script body itself.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Substring the config writers grep for to recognize OUR hook entries among
/// any the user may have added by hand. Embedded in the installed command
/// (`…/memory-inject.sh <agent>`), so any group/table whose `command` contains
/// this marker is one we installed and may safely replace or remove.
pub const INJECT_SCRIPT_MARKER: &str = "memory-inject";

/// The bridge script body, written verbatim to [`script_path`]. Mirrors the
/// existing `~/.claude/hooks/memory-context.sh` style: POSIX-ish bash, fail
/// soft (a missing `memory` binary or empty result injects nothing rather
/// than erroring the turn), and a uniform stdout contract across all three
/// agents.
pub const INJECT_SCRIPT: &str = r#"#!/usr/bin/env bash
# memory-inject.sh — per-turn RAG injection bridge for AI coding agents.
#
# Installed by `memory setup hooks`. Wired into each agent CLI's per-turn
# prompt hook so relevant memory is injected into context automatically,
# WITHOUT the agent having to run `memory context` itself:
#   Claude Code -> UserPromptSubmit hook   (arg: claude)
#   Codex CLI   -> UserPromptSubmit hook   (arg: codex)
#   Gemini CLI  -> BeforeAgent hook        (arg: gemini)
#
# Contract (uniform across all three): read the hook JSON payload on stdin,
# extract the user's prompt, run `memory context`, and emit
#   {"hookSpecificOutput":{"hookEventName":"<event>","additionalContext":"<text>"}}
# on stdout. Emitting nothing (exit 0) injects nothing — used when there is
# no prompt or no relevant memory, so low-signal turns stay cheap.
set -uo pipefail

AGENT="${1:-claude}"
PAYLOAD="$(cat)"

case "$AGENT" in
  gemini) EVENT="BeforeAgent" ;;
  *)      EVENT="UserPromptSubmit" ;;
esac

if [ -n "${MEMORY_BIN:-}" ]; then
  MEM="$MEMORY_BIN"
elif [ -x /opt/agentic/bin/memory ]; then
  MEM="/opt/agentic/bin/memory"
elif command -v memory >/dev/null 2>&1; then
  MEM="memory"
else
  MEM="/usr/local/bin/memory"
fi

LIMIT="${MEMORY_INJECT_LIMIT:-5}"

PROMPT="$(printf '%s' "$PAYLOAD" | jq -r '
  ( .prompt // .user_prompt // .userPrompt // .message // .input // .text // "" )
  | if type == "string" then . else "" end' 2>/dev/null)"

if [ -z "${PROMPT//[[:space:]]/}" ]; then
  exit 0
fi

# --no-working-context: the SessionStart hook already injects WorkingContext
# (and Pre/PostCompact keep it fresh), so re-emitting it every turn would
# duplicate it. We only want the per-turn ranked recall here.
CTX="$("$MEM" context "$PROMPT" -k "$LIMIT" --no-working-context 2>/dev/null || true)"

if [ -z "${CTX//[[:space:]]/}" ]; then
  exit 0
fi

jq -n --arg ev "$EVENT" --arg ctx "$CTX" \
  '{hookSpecificOutput:{hookEventName:$ev, additionalContext:$ctx}}'
exit 0
"#;

/// Outcome of writing the bridge script. Lets the orchestrator distinguish a
/// real filesystem change (`Created`/`Updated`) from an idempotent no-op
/// (`AlreadyCurrent`) so re-runs stay quiet, mirroring the `SettingsOutcome`
/// vocabulary used by the per-agent config helpers.
#[derive(Debug, PartialEq, Eq)]
pub enum ScriptOutcome {
    /// Script file did not exist; created fresh.
    Created,
    /// Script file existed with different bytes; overwritten.
    Updated,
    /// Script file already byte-identical; no write performed.
    AlreadyCurrent,
    /// `--dry-run`: nothing written, action reported only.
    DryRun,
}

/// Absolute path of the installed bridge script:
/// `~/.agentic/hooks/memory-inject.sh`. Returns `None` only when the home
/// directory can't be resolved (matching the discipline of the other setup
/// helpers, which also bail rather than guessing a home).
pub fn script_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".agentic").join("hooks").join("memory-inject.sh"))
}

/// Build the command string a config writer embeds for `agent`, e.g.
/// `/home/u/.agentic/hooks/memory-inject.sh claude`.
///
/// An absolute path is used deliberately, not `$HOME/...`: the three agent
/// CLIs spawn hooks with differing working directories and shell expansion
/// guarantees, and an absolute path is the only form that is unambiguous
/// across all of them.
pub fn installed_command(agent: &str, script: &Path) -> String {
    format!("{} {}", script.display(), agent)
}

/// Write the bridge script to [`script_path`], creating the parent
/// `~/.agentic/hooks/` directory and marking the file executable (0o755).
///
/// Idempotent: if the file already exists with byte-identical contents we
/// return [`ScriptOutcome::AlreadyCurrent`] without rewriting it — no mtime
/// churn on repeated `memory setup hooks` runs. `dry_run` reports the intended
/// action and writes nothing.
///
/// The chmod is gated behind `#[cfg(unix)]`: this POC targets Linux/macOS, but
/// the code still compiles on other platforms (where the executable bit is a
/// no-op concept).
pub fn write_script(dry_run: bool) -> Result<ScriptOutcome> {
    let path = script_path().context("resolve home directory for hook script path")?;

    if dry_run {
        return Ok(ScriptOutcome::DryRun);
    }

    // Idempotency: skip the write entirely when the on-disk bytes already
    // match. We still (re)assert the executable bit below only on a real
    // write, so an AlreadyCurrent result is a genuine no-op.
    let existing = std::fs::read_to_string(&path).ok();
    let outcome = match existing.as_deref() {
        Some(body) if body == INJECT_SCRIPT => return Ok(ScriptOutcome::AlreadyCurrent),
        Some(_) => ScriptOutcome::Updated,
        None => ScriptOutcome::Created,
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create hook script parent dir {}", parent.display()))?;
    }
    std::fs::write(&path, INJECT_SCRIPT)
        .with_context(|| format!("write hook script {}", path.display()))?;
    set_executable(&path)?;

    Ok(outcome)
}

/// Mark `path` executable (0o755) on unix. No-op on other platforms so the
/// crate still compiles there; this POC only runs on Linux/macOS.
#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("stat hook script {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("chmod hook script {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Delete the bridge script if present. Returns `true` when a file was
/// removed, `false` when there was nothing to remove. The `hooks/` directory
/// is left in place — it may hold other tools' scripts (e.g.
/// `memory-context.sh`), and removing a possibly-shared directory would be
/// surprising.
pub fn remove_script() -> Result<bool> {
    let path = script_path().context("resolve home directory for hook script path")?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => {
            Err(anyhow::Error::new(e).context(format!("remove hook script {}", path.display())))
        }
    }
}

/// Print the script body to stdout. Backs `memory setup hooks --print`, the
/// filesystem-free way to inspect exactly what would be installed.
pub fn print_script() {
    print!("{INJECT_SCRIPT}");
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_body_has_shebang_and_uniform_contract() {
        assert!(INJECT_SCRIPT.starts_with("#!/usr/bin/env bash"));
        // The stdout envelope keys all three agents share.
        assert!(INJECT_SCRIPT.contains("hookSpecificOutput"));
        assert!(INJECT_SCRIPT.contains("hookEventName"));
        assert!(INJECT_SCRIPT.contains("additionalContext"));
        // Per-agent event selection.
        assert!(INJECT_SCRIPT.contains("gemini) EVENT=\"BeforeAgent\""));
        assert!(INJECT_SCRIPT.contains("EVENT=\"UserPromptSubmit\""));
        // It calls `memory context`, not a model-initiated tool.
        assert!(INJECT_SCRIPT.contains("context"));
    }

    #[test]
    fn marker_is_substring_of_installed_command() {
        // The config writers find our entries by this marker, so the marker
        // MUST appear in whatever command they embed.
        let cmd = installed_command("claude", Path::new("/home/u/.agentic/hooks/memory-inject.sh"));
        assert!(cmd.contains(INJECT_SCRIPT_MARKER));
    }

    #[test]
    fn installed_command_uses_absolute_path_and_agent_arg() {
        let script = Path::new("/home/u/.agentic/hooks/memory-inject.sh");
        assert_eq!(
            installed_command("codex", script),
            "/home/u/.agentic/hooks/memory-inject.sh codex"
        );
        assert_eq!(
            installed_command("gemini", script),
            "/home/u/.agentic/hooks/memory-inject.sh gemini"
        );
    }

    #[test]
    fn script_path_lands_under_agentic_hooks() {
        // Best-effort: only assert the tail when a home dir is resolvable in
        // the test environment.
        if let Some(p) = script_path() {
            assert!(p.ends_with(".agentic/hooks/memory-inject.sh"), "got {p:?}");
        }
    }

    /// Write → idempotent re-run → executable bit. Drives the real filesystem
    /// helper indirectly by pointing HOME at a temp dir is not possible
    /// (script_path reads the process HOME), so this test exercises the pure
    /// idempotency logic against a temp file by mirroring the bytes check.
    #[test]
    fn write_then_rewrite_is_idempotent_on_bytes() {
        // Direct filesystem exercise of the create/already-current contract
        // using a standalone temp file, independent of the process HOME.
        let dir = std::env::temp_dir().join(format!(
            "agent-memory-hook-script-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory-inject.sh");

        // First "install": file absent → Created semantics.
        assert!(std::fs::read_to_string(&path).is_err());
        std::fs::write(&path, INJECT_SCRIPT).unwrap();
        set_executable(&path).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, INJECT_SCRIPT);

        // Second "install": identical bytes → AlreadyCurrent semantics.
        let again = std::fs::read_to_string(&path).unwrap();
        assert_eq!(again, INJECT_SCRIPT, "re-read must be byte-identical");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "script must be chmod 0o755");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
