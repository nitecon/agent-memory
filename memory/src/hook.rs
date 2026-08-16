//! Runtime entrypoint for `memory hook` — the per-turn RAG injection bridge.
//!
//! `memory setup hooks` installs `memory hook --agent <agent>` into each agent
//! CLI's per-turn hook (Claude `UserPromptSubmit`, Gemini `BeforeAgent`, Codex
//! `UserPromptSubmit`). On every turn the agent runs that command, piping the
//! hook JSON payload on stdin. This module reads it, extracts the prompt, runs
//! the same retrieval `memory context --no-working-context` performs, and emits
//!
//! ```json
//! {"hookSpecificOutput":{"hookEventName":"<event>","additionalContext":"<text>"}}
//! ```
//!
//! on stdout for the agent to inject. It replaces the old shared bash script
//! (`memory-inject.sh`), which broke on Windows (no bash/jq, `.sh` opened in an
//! editor). A binary subcommand is cross-platform by construction.
//!
//! MASTER RULE: this path is fail-soft. [`run`] never returns an error and
//! never panics — any failure (bad JSON, no prompt, DB error, empty result)
//! emits nothing and returns. Emitting nothing on stdout injects nothing, which
//! is exactly the desired behavior for low-signal turns and must NEVER block
//! the user's prompt. stderr may carry diagnostics; stdout carries only the
//! envelope or nothing.

use std::io::Read;

use rusqlite::Connection;

use crate::cli;
use crate::config::Config;
use crate::project;

/// Set to a false-like value by non-interactive curation subprocesses that
/// already carry complete context and must not recursively retrieve memory.
pub const HOOK_ENABLE_ENV: &str = "AGENT_MEMORY_HOOK";

fn hook_enabled(value: Option<&str>) -> bool {
    !matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

/// Hook payload fields we probe, in precedence order. The first one whose value
/// is a non-empty string wins. Mirrors the old bash script's jq extraction so
/// behavior is unchanged across the scriptless migration.
const PROMPT_FIELDS: &[&str] = &[
    "prompt",
    "user_prompt",
    "userPrompt",
    "message",
    "input",
    "text",
];

/// Map an agent name to the hook event name the agent expects echoed back in
/// the envelope: Gemini uses `BeforeAgent`, everything else `UserPromptSubmit`.
fn event_name(agent: &str) -> &'static str {
    if agent == "gemini" {
        "BeforeAgent"
    } else {
        "UserPromptSubmit"
    }
}

/// Extract the user prompt from a raw hook JSON payload. Tries [`PROMPT_FIELDS`]
/// in order; returns the first whose value is a non-empty (post-trim) string.
///
/// Returns `None` when the payload isn't valid JSON, has no matching field, or
/// every candidate is absent/non-string/blank. Pure (no IO) so it is unit
/// testable without a DB or stdin.
fn extract_prompt(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let obj = value.as_object()?;
    for field in PROMPT_FIELDS {
        if let Some(s) = obj.get(*field).and_then(serde_json::Value::as_str) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Build the `hookSpecificOutput` envelope as a JSON string. Uses `serde_json`
/// so `event` and `context` are escaped correctly. Pure — unit testable.
fn build_envelope(event: &str, context: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": context,
        }
    })
    .to_string()
}

/// Runtime hook entrypoint. Fail-soft per the module MASTER RULE: any failure
/// emits nothing and returns; this function never errors and never panics.
///
/// Steps: read stdin → [`extract_prompt`] → run the shared
/// [`cli::retrieve_context_block`] retrieval (`no_working_context = true`, since
/// the SessionStart hook already injects WorkingContext) → emit
/// [`build_envelope`] on stdout. Any empty/blank result short-circuits to "emit
/// nothing".
pub fn run(agent: &str, limit: usize, conn: &Connection, config: &Config) {
    if !hook_enabled(std::env::var(HOOK_ENABLE_ENV).ok().as_deref()) {
        return;
    }
    let mut payload = String::new();
    if std::io::stdin().read_to_string(&mut payload).is_err() {
        return;
    }

    let Some(prompt) = extract_prompt(&payload) else {
        return;
    };
    if prompt.trim().is_empty() {
        return;
    }

    // The hook never narrows to a single project (`only = None`) and never
    // disables boosts; cwd auto-detection drives the project boost just like an
    // interactive `memory context`. Failure to detect a project is fine (None).
    let cwd_project = project::project_ident_from_cwd().ok();
    let block = match cli::retrieve_context_block(
        conn,
        config,
        &prompt,
        limit,
        /* no_working_context */ true,
        /* project */ None,
        cwd_project.as_deref(),
        /* no_project_boost */ false,
        /* only */ None,
    ) {
        Ok(block) => block,
        Err(_) => return,
    };

    if block.trim().is_empty() {
        return;
    }

    println!("{}", build_envelope(event_name(agent), &block));
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_prompt_reads_each_field() {
        for field in PROMPT_FIELDS {
            let payload = format!(r#"{{"{field}": "hello {field}"}}"#);
            assert_eq!(
                extract_prompt(&payload).as_deref(),
                Some(format!("hello {field}").as_str()),
                "field {field} should be read"
            );
        }
    }

    #[test]
    fn extract_prompt_honors_precedence() {
        // `prompt` wins over every later field.
        let payload =
            r#"{"text": "from text", "message": "from message", "prompt": "from prompt"}"#;
        assert_eq!(extract_prompt(payload).as_deref(), Some("from prompt"));

        // First non-empty wins: empty `prompt` falls through to `message`.
        let payload = r#"{"prompt": "   ", "message": "from message", "text": "from text"}"#;
        assert_eq!(extract_prompt(payload).as_deref(), Some("from message"));
    }

    #[test]
    fn extract_prompt_returns_none_for_missing_empty_or_malformed() {
        assert_eq!(extract_prompt("{}"), None);
        assert_eq!(extract_prompt(r#"{"prompt": ""}"#), None);
        assert_eq!(extract_prompt(r#"{"prompt": "   \n\t "}"#), None);
        // Non-string value is ignored.
        assert_eq!(extract_prompt(r#"{"prompt": 42}"#), None);
        // Malformed JSON.
        assert_eq!(extract_prompt("not json"), None);
        assert_eq!(extract_prompt(""), None);
        // JSON that isn't an object.
        assert_eq!(extract_prompt(r#"["prompt"]"#), None);
    }

    #[test]
    fn build_envelope_is_valid_json_with_expected_keys() {
        let out = build_envelope("UserPromptSubmit", "some context");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let inner = &parsed["hookSpecificOutput"];
        assert_eq!(inner["hookEventName"], "UserPromptSubmit");
        assert_eq!(inner["additionalContext"], "some context");
    }

    #[test]
    fn build_envelope_escapes_special_characters() {
        // Quotes, newlines, and backslashes in the context must round-trip.
        let ctx = "line1\n\"quoted\"\tand\\back";
        let out = build_envelope("BeforeAgent", ctx);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["hookSpecificOutput"]["additionalContext"], ctx);
    }

    #[test]
    fn event_name_maps_gemini_to_before_agent_else_user_prompt_submit() {
        assert_eq!(event_name("gemini"), "BeforeAgent");
        assert_eq!(event_name("claude"), "UserPromptSubmit");
        assert_eq!(event_name("codex"), "UserPromptSubmit");
        assert_eq!(event_name("anything-else"), "UserPromptSubmit");
    }

    #[test]
    fn hook_enable_env_accepts_false_like_opt_outs() {
        for value in ["0", "false", "FALSE", "no", "off", " OFF "] {
            assert!(!hook_enabled(Some(value)), "{value:?} should disable hook");
        }
        for value in ["1", "true", "yes", "on", ""] {
            assert!(hook_enabled(Some(value)), "{value:?} should enable hook");
        }
        assert!(hook_enabled(None));
    }
}
