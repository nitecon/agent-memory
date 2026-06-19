//! Scriptless hook-command management for `memory setup hooks`.
//!
//! `memory setup hooks` wires three agent CLIs to inject relevant memory into
//! their context automatically on every turn, replacing the rule-based "the
//! agent must remember to run `memory context`" approach. Instead of a fragile
//! shared bash script (the previous mechanism — see [`LEGACY_SCRIPT_MARKER`]),
//! each CLI invokes the `memory` binary's own [`crate::hook`] subcommand,
//! passing its agent name:
//!
//!   - Claude Code → `UserPromptSubmit` hook → `memory hook --agent claude`
//!   - Codex CLI   → `UserPromptSubmit` hook → `memory hook --agent codex`
//!   - Gemini CLI  → `BeforeAgent` hook      → `memory hook --agent gemini`
//!
//! A binary subcommand is cross-platform by construction: no `bash`, no `jq`,
//! and no `.sh` file-association surprises (on Windows the old `memory-inject.sh`
//! opened in an editor instead of running). The subcommand reads the hook JSON
//! payload on stdin, runs `memory context`, and emits a `hookSpecificOutput`
//! envelope with `additionalContext` set to the retrieved memory. Emitting
//! nothing (exit 0) injects nothing, which keeps low-signal turns cheap.
//!
//! This module owns the single source of truth for the command string the
//! per-agent config writers embed, plus the two markers used to find our
//! entries: [`HOOK_MARKER`] for new installs and [`LEGACY_SCRIPT_MARKER`] so
//! upgrades can strip stale script-based entries from before this change.

use std::path::PathBuf;

/// Substring every command we install contains (`memory hook --agent <agent>`).
/// The config writers grep for it to recognize OUR hook entries among any the
/// user added by hand, so any group/table whose `command` contains this marker
/// is one we installed and may safely replace or remove.
pub const HOOK_MARKER: &str = "hook --agent";

/// Substring of the OLD script-based installs (`…/memory-inject.sh <agent>`).
/// Retained so `memory setup hooks` upgrades can recognize and strip a stale
/// pre-scriptless entry before installing the new binary command, and so the
/// menu still reports "installed" while a user is mid-upgrade.
pub const LEGACY_SCRIPT_MARKER: &str = "memory-inject";

/// Build the command string a config writer embeds for `agent`, e.g.
/// (unix) `"/opt/agentic/bin/memory" hook --agent codex` or
/// (windows) `"C:\Users\me\.agentic\bin\memory.exe" hook --agent codex`.
///
/// The executable path is resolved via [`std::env::current_exe`] so the
/// installed command points at the exact binary the user ran `setup` with. On
/// failure (rare — e.g. the binary was unlinked), we fall back to the bare
/// program name on `PATH` (`memory` / `memory.exe`).
///
/// The path is wrapped in double quotes so install paths containing spaces —
/// common on Windows under `C:\Users\<name with space>\…` — survive being run
/// through a shell. The string always contains [`HOOK_MARKER`].
pub fn installed_command(agent: &str) -> String {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(default_program_name);
    format!("\"{exe}\" hook --agent {agent}")
}

/// Bare program name used when [`std::env::current_exe`] can't resolve the
/// running binary: `memory.exe` on Windows, `memory` elsewhere. Relies on the
/// binary being discoverable on `PATH`.
fn default_program_name() -> String {
    if cfg!(windows) {
        "memory.exe".to_string()
    } else {
        "memory".to_string()
    }
}

/// Absolute path of a bridge script left by an OLD script-based install:
/// `~/.agentic/hooks/memory-inject.sh`. Returned so the orchestrator can
/// best-effort delete a stale script during upgrade/remove. `None` only when
/// the home directory can't be resolved.
pub fn legacy_script_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".agentic").join("hooks").join("memory-inject.sh"))
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_command_contains_hook_marker_and_shape() {
        let cmd = installed_command("codex");
        // The config writers find our entries by this marker, so it MUST appear.
        assert!(cmd.contains(HOOK_MARKER), "missing marker in {cmd:?}");
        // Full per-agent shape.
        assert!(cmd.contains("hook --agent codex"), "wrong shape: {cmd:?}");
    }

    #[test]
    fn installed_command_quotes_executable_path() {
        let cmd = installed_command("gemini");
        // The executable path is the first token and must be double-quoted so
        // paths with spaces survive a shell.
        assert!(cmd.starts_with('"'), "exe path not quoted: {cmd:?}");
        // Closing quote precedes the ` hook --agent` tail.
        assert!(
            cmd.contains("\" hook --agent gemini"),
            "closing quote missing: {cmd:?}"
        );
    }

    #[test]
    fn installed_command_varies_by_agent() {
        assert!(installed_command("claude").ends_with("hook --agent claude"));
        assert!(installed_command("gemini").ends_with("hook --agent gemini"));
        assert!(installed_command("codex").ends_with("hook --agent codex"));
    }

    #[test]
    fn markers_are_the_documented_constants() {
        assert_eq!(HOOK_MARKER, "hook --agent");
        assert_eq!(LEGACY_SCRIPT_MARKER, "memory-inject");
    }

    #[test]
    fn legacy_script_path_lands_under_agentic_hooks() {
        if let Some(p) = legacy_script_path() {
            assert!(p.ends_with(".agentic/hooks/memory-inject.sh"), "got {p:?}");
        }
    }
}
