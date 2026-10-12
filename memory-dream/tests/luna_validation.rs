//! Explicit opt-in, read-only validation against the authenticated Codex CLI.
use agent_memory::db::{models::Memory, open_database};
use memory_dream::dream::{condense, project_review};
use memory_dream::inference::HeadlessInference;

const COMMAND: &str = "codex exec --ignore-user-config --ephemeral --sandbox read-only -c project_doc_max_bytes=0 -c 'model_reasoning_effort=\"low\"' -c 'developer_instructions=\"Classify only the supplied memories. Never call tools or inspect files. Return only the requested output.\"' --model {model} - {prompt_stdin}";

#[test]
#[ignore = "Requires authenticated Codex access to gpt-6-luna; incurs model usage"]
fn luna_read_only_review_covers_seventy_ids_with_large_stdin_prompt() {
    let mut conn = open_database(std::path::Path::new(":memory:")).unwrap();
    let mut memories = Vec::new();
    for i in 0..70 {
        let body = match i {
            0 => "Pin cu128 on sm_120 because PyTorch nightly cu130 silently falls back to cu128 kernels.".into(),
            1 => "On sm_120 (Blackwell), use cu128 explicitly: PyTorch nightly cu130 silently falls back to cu128 kernels.".into(),
            2 => "v1.2.3 shipped; CI passed; release tag v1.2.3 was pushed.".into(),
            _ => {
                let fact = format!("Workflow {i}: inspect /srv/service-{i}/settings.toml before restarting service-{i}; preserve the configured retry delay because its downstream client requires that delay. ");
                // Exercise stdin beyond Linux's per-argument limit, below the
                // existing project-review content budget. Repetition is data.
                fact.repeat(12)
            }
        };
        let mut m = Memory::new(
            body,
            None,
            Some("validation".into()),
            None,
            None,
            Some("project".into()),
        );
        m.id = format!("aaaaaaaa-0000-1111-2222-{i:012}");
        memories.push(m);
    }
    let prompt = project_review::prompt::build_project_review_prompt(Some("validation"), &memories);
    assert!(prompt.len() > 131_072);
    assert!(
        memories.iter().map(|m| m.content.len()).sum::<usize>() < project_review::MAX_PROJECT_CHARS
    );
    let backend = HeadlessInference::new_with_model(COMMAND, "gpt-6-luna", 600_000).unwrap();
    // Only disposable fixture rows; no production database or gateway writes.
    for m in &memories {
        agent_memory::db::queries::insert_memory(&conn, m).unwrap();
    }
    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_revisions", [], |r| r.get(0))
        .unwrap();
    let result = project_review::run_project(
        &mut conn,
        &backend,
        Some("validation"),
        memories,
        "headless/codex/gpt-6-luna",
        &std::env::temp_dir(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(result.stats.failed, 0);
    assert_eq!(
        result.stats.kept
            + result.stats.dropped
            + result.stats.merged
            + result.stats.superseded
            + result.stats.extracted
            + result.stats.deprecated
            + result.stats.contradictions,
        70
    );
    assert!(result.stats.dropped >= 1);
    assert!(result.stats.merged >= 1);
    assert!(result
        .survivors
        .iter()
        .any(|m| m.content.contains("cu128") && m.content.contains("sm_120")));
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM memory_revisions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        before
    );
    println!("Luna provenance: headless/codex/gpt-6-luna; stdin_bytes={}; decisions=70/70; dry-run writes=0", prompt.len());
}

#[test]
#[ignore = "Requires authenticated Codex access to gpt-6-luna; incurs model usage"]
fn luna_single_memory_contract_preserves_useful_guidance() {
    let backend = HeadlessInference::new_with_model(COMMAND, "gpt-6-luna", 600_000).unwrap();
    let memory = Memory::new(
        "Use tabs because their display width follows the reader's editor settings.".into(),
        None,
        Some("validation".into()),
        None,
        None,
        Some("user".into()),
    );
    let decision = condense::run_per_memory(&backend, &memory).unwrap();
    assert!(matches!(
        decision,
        condense::Decision::Skip | condense::Decision::Rewrite { .. }
    ));
}
