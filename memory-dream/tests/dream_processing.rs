use std::sync::Mutex;

use agent_memory::db::{models::Memory, open_database, queries as q};
use memory_dream::dream::{run, DreamConfig, DreamMode};
use memory_dream::inference::{Inference, InferenceError};
use rusqlite::Connection;

struct Model<F> {
    prompts: Mutex<Vec<String>>,
    respond: F,
}

impl<F: Fn(&str) -> String + Send + Sync> Inference for Model<F> {
    fn generate(&self, prompt: &str, _: u32) -> Result<String, InferenceError> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        Ok((self.respond)(prompt))
    }
}

fn model<F: Fn(&str) -> String + Send + Sync>(respond: F) -> Model<F> {
    Model {
        prompts: Mutex::new(Vec::new()),
        respond,
    }
}

fn keep_review(prompt: &str) -> String {
    let decisions: serde_json::Map<_, _> = prompt
        .lines()
        .filter_map(|line| line.strip_prefix("id: "))
        .map(|id| (id.to_string(), serde_json::json!({"action": "keep"})))
        .collect();
    serde_json::json!({"decisions": decisions}).to_string()
}

fn insert(conn: &Connection, id: &str, body: &str) {
    let mut m = Memory::new(
        body.into(),
        None,
        Some("test".into()),
        None,
        None,
        Some("user".into()),
    );
    m.id = id.into();
    q::insert_memory(conn, &m).unwrap();
}

fn revision(conn: &Connection, id: &str) -> i64 {
    conn.query_row(
        "SELECT current_revision FROM memory_concepts WHERE memory_id = ?1",
        [id],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn successful_keep_and_enrichment_skip_two_ordinary_passes_without_churn() {
    let mut conn = open_database(std::path::Path::new(":memory:")).unwrap();
    insert(
        &conn,
        "keep",
        "Prefer tabs because they preserve user-configured indentation width.",
    );
    let original = "Pin cu128: PyTorch nightly cu130 falls back to cu128 on sm_120.";
    let enriched =
        "Pin cu128 on sm_120 to avoid fallback: PyTorch nightly cu130 falls back to cu128.";
    insert(&conn, "enrich", original);
    let inf = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            keep_review(prompt)
        } else if prompt.contains(original) {
            enriched.to_string()
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    let cache = std::env::temp_dir();
    let mut cfg = DreamConfig::new(DreamMode::Apply, "headless/codex/gpt-6-luna", &cache);
    let first = run(&mut conn, &inf, &cfg).unwrap();
    assert_eq!(first.failed, 0);
    assert_eq!(first.kept, 1);
    assert_eq!(first.rewritten, 1);
    assert_eq!(
        q::get_memory_by_id(&conn, "enrich").unwrap().content,
        enriched
    );
    let before = (
        revision(&conn, "keep"),
        revision(&conn, "enrich"),
        q::get_memory_by_id(&conn, "keep").unwrap().updated_at,
    );
    let calls = inf.prompts.lock().unwrap().len();
    // A gateway write on either subsequent pass would fail and be visible.
    let gateway = agent_memory::config::GatewayConfig {
        base_url: Some("http://127.0.0.1:1".into()),
        api_key: Some("fixture".into()),
        auto_sync: Some(true),
        ..Default::default()
    };
    cfg.gateway_config = Some(&gateway);
    for _ in 0..2 {
        let pass = run(&mut conn, &inf, &cfg).unwrap();
        assert_eq!(pass.total_walked, 0);
        assert_eq!(pass.rewritten + pass.failed, 0);
    }
    assert_eq!(inf.prompts.lock().unwrap().len(), calls);
    assert_eq!(
        before,
        (
            revision(&conn, "keep"),
            revision(&conn, "enrich"),
            q::get_memory_by_id(&conn, "keep").unwrap().updated_at
        )
    );
}

#[test]
fn failed_condensation_retries_while_successful_context_stays_untouched() {
    let mut conn = open_database(std::path::Path::new(":memory:")).unwrap();
    insert(
        &conn,
        "good",
        "Use tabs because indentation width is configurable.",
    );
    insert(&conn, "bad", "Retry me after an inference failure.");
    let inf = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            keep_review(prompt)
        } else if prompt.contains("Retry me") {
            "I cannot help.".into()
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    let cache = std::env::temp_dir();
    let cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().failed, 1);
    let good_revision = revision(&conn, "good");
    assert!(q::get_memory_by_id(&conn, "bad")
        .unwrap()
        .condenser_version
        .is_none());
    for _ in 0..2 {
        let result = run(&mut conn, &inf, &cfg).unwrap();
        assert_eq!(result.total_walked, 1);
        assert_eq!(result.failed, 1);
        assert_eq!(revision(&conn, "good"), good_revision);
    }
    assert_eq!(inf.prompts.lock().unwrap().len(), 7); // 3 calls, then 2 + 2
}

#[test]
fn failed_review_is_retryable_even_when_condensation_keeps() {
    let mut conn = open_database(std::path::Path::new(":memory:")).unwrap();
    insert(&conn, "keep", "Keep useful supported context.");
    let inf = model(|_: &str| "KEEP_UNCHANGED".into()); // invalid review JSON
    let cache = std::env::temp_dir();
    let cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    for _ in 0..2 {
        let result = run(&mut conn, &inf, &cfg).unwrap();
        assert_eq!(result.total_walked, 1);
        assert_eq!(result.failed, 1);
        assert!(q::get_memory_by_id(&conn, "keep")
            .unwrap()
            .condenser_version
            .is_none());
    }
}

#[test]
fn new_evidence_can_replace_processed_target_with_larger_combined_context() {
    let mut conn = open_database(std::path::Path::new(":memory:")).unwrap();
    let old = "Pin cu128 on sm_120 to avoid silent fallback.";
    let new = "PyTorch nightly cu130 silently falls back to cu128 kernels on sm_120 (Blackwell); pin cu128 explicitly.";
    let combined = "Pin cu128 on sm_120 (Blackwell): PyTorch nightly cu130 silently falls back to cu128 kernels.";
    assert!(combined.len() > old.len() * 3 / 2);
    assert!(combined.len() <= (old.len() + new.len()) * 3 / 4);
    insert(&conn, "old", old);
    insert(
        &conn,
        "unrelated",
        "Prefer tabs because indentation width is configurable.",
    );
    let keep = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            keep_review(prompt)
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    let cache = std::env::temp_dir();
    let cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    assert_eq!(run(&mut conn, &keep, &cfg).unwrap().failed, 0);
    let untouched = (
        revision(&conn, "unrelated"),
        q::get_memory_by_id(&conn, "unrelated").unwrap().updated_at,
    );
    insert(&conn, "new", new);
    let merge = model(|prompt: &str| {
        assert!(prompt.contains(old) && prompt.contains(new));
        assert!(prompt.contains("pending memory ids: new."));
        serde_json::json!({"decisions": {
            "old": {"action": "supersede_by", "content": combined},
            "new": {"action": "merge_into", "target_id": "old"},
            "unrelated": {"action": "keep"}
        }})
        .to_string()
    });
    let result = run(&mut conn, &merge, &cfg).unwrap();
    assert_eq!(result.total_walked, 1);
    assert_eq!(result.failed, 0);
    assert_eq!(result.review_merged, 1);
    assert_eq!(result.review_superseded, 1);
    assert_eq!(merge.prompts.lock().unwrap().len(), 1);
    let live = q::list_dream_candidates(&conn, Some("test"), None, None, 0).unwrap();
    let target = live.iter().find(|m| m.content == combined).unwrap();
    assert_eq!(
        q::get_memory_by_id(&conn, "new")
            .unwrap()
            .superseded_by
            .as_deref(),
        Some(target.id.as_str())
    );
    let source_edges: i64 = conn.query_row("SELECT COUNT(*) FROM memory_relationships WHERE src_memory_id = ?1 AND relation = 'derived_from'", [&target.id], |r| r.get(0)).unwrap();
    assert_eq!(source_edges, 2);
    assert_eq!(
        untouched,
        (
            revision(&conn, "unrelated"),
            q::get_memory_by_id(&conn, "unrelated").unwrap().updated_at
        )
    );
    assert_eq!(run(&mut conn, &merge, &cfg).unwrap().total_walked, 0);
}

#[test]
fn editing_a_processed_memory_makes_it_pending_again() {
    let mut conn = open_database(std::path::Path::new(":memory:")).unwrap();
    insert(
        &conn,
        "edited",
        "Use tabs because indentation width is configurable.",
    );
    let inf = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            keep_review(prompt)
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    let cache = std::env::temp_dir();
    let cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().failed, 0);
    q::update_content(
        &conn,
        "edited",
        "Use spaces because this repository requires them.",
        None,
        None,
    )
    .unwrap();
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 1);
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 0);
}

#[test]
fn merge_budget_is_three_quarters_of_combined_unicode_characters() {
    use memory_dream::dream::project_review::{parse_response, Decision};
    let mut left = Memory::new("é".repeat(10), None, None, None, None, None);
    let mut right = left.clone();
    left.id = "left".into();
    right.id = "right".into();
    for (length, accepted) in [(15, true), (16, false)] {
        let reply = serde_json::json!({"decisions": {
            "left": {"action": "merge_into", "target_id": "right"},
            "right": {"action": "supersede_by", "content": "é".repeat(length)}
        }})
        .to_string();
        let result = parse_response(&reply, &[left.clone(), right.clone()]);
        assert_eq!(result.is_ok(), accepted);
        if let Ok(decisions) = result {
            assert!(matches!(
                decisions.get("left"),
                Some(Decision::MergeInto { .. })
            ));
        }
    }
}

#[test]
fn a_limit_does_not_review_other_unprocessed_rows_as_context() {
    let mut conn = open_database(std::path::Path::new(":memory:")).unwrap();
    for id in ["a", "b", "c"] {
        insert(&conn, id, &format!("Guidance for subsystem {id}."));
    }
    let inf = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            keep_review(prompt)
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    let cache = std::env::temp_dir();
    let mut cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    cfg.limit = 1;
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 1);
    assert!(inf.prompts.lock().unwrap()[0].contains("Memory count: 1"));
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 1);
    assert!(inf.prompts.lock().unwrap()[2].contains("Memory count: 2"));
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 1);
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 0);
}

#[test]
fn failed_removal_of_processed_context_remains_retryable() {
    let mut conn = open_database(std::path::Path::new(":memory:")).unwrap();
    insert(
        &conn,
        "old",
        "Old operational guidance that was previously curated.",
    );
    let keep = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            keep_review(prompt)
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    let cache = std::env::temp_dir();
    let cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    assert_eq!(run(&mut conn, &keep, &cfg).unwrap().failed, 0);
    insert(
        &conn,
        "new",
        "New evidence supersedes old operational guidance.",
    );
    conn.execute_batch("CREATE TRIGGER block_old_delete BEFORE DELETE ON memories WHEN OLD.id = 'old' BEGIN SELECT RAISE(FAIL, 'fixture deletion failure'); END").unwrap();
    let drop = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            serde_json::json!({"decisions": {"old": {"action":"drop"}, "new": {"action":"keep"}}})
                .to_string()
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    assert_eq!(run(&mut conn, &drop, &cfg).unwrap().failed, 1);
    assert!(q::get_memory_by_id(&conn, "old")
        .unwrap()
        .condenser_version
        .is_none());
    assert!(q::get_memory_by_id(&conn, "new")
        .unwrap()
        .condenser_version
        .is_some());
    assert_eq!(run(&mut conn, &keep, &cfg).unwrap().total_walked, 1);
}
