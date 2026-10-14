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
fn review_transport_keys_preserve_completed_and_review_pending_upgrade_stamps() {
    // The preceding release's curation policy. Response transport changes must
    // preserve these stamps; a deliberate curation policy change may revise it.
    const PREVIOUS_STAMP: &str = "headless/codex/gpt-6-luna:31e6a11c";
    for pending_review in [false, true] {
        let mut conn = open_database(std::path::Path::new(":memory:")).unwrap();
        insert(
            &conn,
            "upgrade",
            "Prefer tabs because indentation width is configurable.",
        );
        let stamp = if pending_review {
            format!("{PREVIOUS_STAMP}:review-pending")
        } else {
            PREVIOUS_STAMP.into()
        };
        q::set_dream_processed(&conn, "upgrade", Some(&stamp)).unwrap();
        let inf = model(|prompt: &str| {
            assert!(
                prompt.contains("Memory count:"),
                "completed condensation must survive upgrade"
            );
            keep_review(prompt)
        });
        let cache = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Apply, "headless/codex/gpt-6-luna", &cache);
        let result = run(&mut conn, &inf, &cfg).unwrap();
        assert_eq!(result.failed, 0);
        assert_eq!(result.total_walked, usize::from(pending_review));
        assert_eq!(
            inf.prompts.lock().unwrap().len(),
            usize::from(pending_review)
        );
        assert_eq!(
            q::get_memory_by_id(&conn, "upgrade")
                .unwrap()
                .condenser_version
                .as_deref(),
            Some(PREVIOUS_STAMP)
        );
        assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 0);
    }
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let mut conn = open_database(&path).unwrap();
    insert(&conn, "keep", "Keep useful supported context.");
    insert(
        &conn,
        "rewrite",
        "Pin cu128 explicitly on sm_120 to avoid fallback kernels.",
    );
    insert(&conn, "failure", "A memory whose condensation must retry.");
    let rewritten = "Pin cu128 on sm_120 to avoid fallback kernels.";
    let inf = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            "invalid review JSON".into()
        } else if prompt.contains("condensation must retry") {
            "I cannot help.".into()
        } else if prompt.contains("cu128") {
            rewritten.into()
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    let cache = std::env::temp_dir();
    let cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    let first = run(&mut conn, &inf, &cfg).unwrap();
    assert_eq!(first.failed, 2); // review and the real condensation failure
    assert_eq!(first.rewritten, 1);
    let before = (revision(&conn, "keep"), revision(&conn, "rewrite"));
    for _ in 0..2 {
        drop(conn);
        conn = open_database(&path).unwrap();
        let result = run(&mut conn, &inf, &cfg).unwrap();
        assert_eq!(result.total_walked, 3);
        assert_eq!(result.failed, 2);
        assert_eq!(result.rewritten + result.kept, 0);
        assert_eq!(
            before,
            (revision(&conn, "keep"), revision(&conn, "rewrite"))
        );
        assert_eq!(
            q::get_memory_by_id(&conn, "rewrite").unwrap().content,
            rewritten
        );
        assert!(q::get_memory_by_id(&conn, "keep")
            .unwrap()
            .condenser_version
            .unwrap()
            .ends_with(":review-pending"));
        assert!(q::get_memory_by_id(&conn, "failure")
            .unwrap()
            .condenser_version
            .is_none());
    }
    assert_eq!(inf.prompts.lock().unwrap().len(), 8); // 4 calls, then 2 + 2
    let recovery = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            keep_review(prompt)
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    assert_eq!(run(&mut conn, &recovery, &cfg).unwrap().failed, 0);
    assert_eq!(recovery.prompts.lock().unwrap().len(), 2); // review + failed body only
    assert_eq!(run(&mut conn, &recovery, &cfg).unwrap().total_walked, 0);
    // Completed processing still becomes stale after an edit or model change.
    q::update_content(
        &conn,
        "rewrite",
        "Pin cu128 on sm_120; preserve the CUDA constraint.",
        None,
        None,
    )
    .unwrap();
    assert_eq!(run(&mut conn, &recovery, &cfg).unwrap().total_walked, 1);
    let changed_cfg = DreamConfig::new(DreamMode::Apply, "other:model", &cache);
    assert_eq!(
        run(&mut conn, &recovery, &changed_cfg)
            .unwrap()
            .total_walked,
        3
    );
}

fn fixture_gateway(
    replies: Vec<serde_json::Value>,
) -> (
    agent_memory::config::GatewayConfig,
    std::thread::JoinHandle<Vec<serde_json::Value>>,
) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in replies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < header_end + length {
                let mut buf = [0; 4096];
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
            }
            requests.push(if length == 0 {
                serde_json::Value::Null
            } else {
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
            });
            let body = response.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
        requests
    });
    (
        agent_memory::config::GatewayConfig {
            base_url: Some(format!("http://{address}")),
            api_key: Some("fixture".into()),
            auto_sync: Some(true),
            okf_sync: Some(false),
        },
        thread,
    )
}

fn push_reply(action: &str, body: &str, revision: i64) -> serde_json::Value {
    serde_json::json!({"project_ident":"test", "server_revision":revision, "results":[{
        "local_memory_id":"rewrite", "gateway_memory_id":"gw-rewrite", "server_revision":revision,
        "action":action, "content_hash":agent_memory::sync::memory_content_hash(body, "user", &[])
    }]})
}

#[test]
fn persistent_successes_skip_after_gateway_push_and_same_content_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let mut conn = open_database(&path).unwrap();
    let original = "Pin cu128 explicitly on sm_120 to avoid fallback kernels.";
    let rewritten = "Pin cu128 on sm_120 to avoid fallback kernels.";
    let kept = "Prefer tabs because their display width is configurable.";
    insert(&conn, "rewrite", original);
    insert(&conn, "keep", kept);
    let remote = |revision| {
        serde_json::json!({"project_ident":"test", "server_revision":revision,
        "memories":[
            {"project_ident":"test", "gateway_memory_id":"gw-rewrite", "server_revision":revision,
             "content":rewritten,"memory_type":"user", "content_hash":agent_memory::sync::memory_content_hash(rewritten, "user", &[])},
            {"project_ident":"test", "gateway_memory_id":"gw-keep", "server_revision":revision,
             "content":kept,"memory_type":"user", "content_hash":agent_memory::sync::memory_content_hash(kept, "user", &[])}
        ]})
    };
    let (gateway, server) = fixture_gateway(vec![
        push_reply("created", rewritten, 1),
        serde_json::json!({"projects":["test"]}),
        remote(2),
        serde_json::json!({"projects":["test"]}),
        remote(3),
    ]);
    let cache = std::env::temp_dir();
    let mut cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    cfg.gateway_config = Some(&gateway);
    let inf = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            keep_review(prompt)
        } else if prompt.contains(original) {
            rewritten.into()
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    let first = run(&mut conn, &inf, &cfg).unwrap();
    assert_eq!((first.rewritten, first.kept, first.failed), (1, 1, 0));
    let before: Vec<_> = ["keep", "rewrite"]
        .iter()
        .map(|id| {
            (
                revision(&conn, id),
                q::get_memory_by_id(&conn, id).unwrap().updated_at,
            )
        })
        .collect();
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/release/memory");
    for _ in 0..2 {
        drop(conn);
        conn = open_database(&path).unwrap();
        // Use the actual CLI pull/reconciliation path, not a direct stamp edit.
        let pull = std::process::Command::new(&binary)
            .args(["pull", "--all"])
            .env("AGENT_MEMORY_DIR", dir.path())
            .env(
                "AGENT_MEMORY_GATEWAY_URL",
                gateway.base_url.as_deref().unwrap(),
            )
            .env("AGENT_MEMORY_GATEWAY_API_KEY", "fixture")
            .env("AGENT_MEMORY_GATEWAY_OKF", "false")
            .env("AGENT_MEMORY_NO_UPDATE", "1")
            .output()
            .unwrap();
        assert!(
            pull.status.success(),
            "pull failed: {} {}",
            String::from_utf8_lossy(&pull.stdout),
            String::from_utf8_lossy(&pull.stderr)
        );
        let pass = run(&mut conn, &inf, &cfg).unwrap();
        assert_eq!((pass.total_walked, pass.rewritten, pass.failed), (0, 0, 0));
        assert_eq!(inf.prompts.lock().unwrap().len(), 3);
        for (i, id) in ["keep", "rewrite"].iter().enumerate() {
            assert_eq!(
                before[i],
                (
                    revision(&conn, id),
                    q::get_memory_by_id(&conn, id).unwrap().updated_at
                )
            );
        }
    }
    assert_eq!(
        q::get_memory_gateway_sync(&conn, "rewrite")
            .unwrap()
            .unwrap()
            .last_seen_server_revision,
        3
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0]["memories"][0]["content"], rewritten);
}

#[test]
fn gateway_rejected_rewrite_remains_unstamped_and_retries_after_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let mut conn = open_database(&path).unwrap();
    let original = "Pin cu128 explicitly on sm_120 to avoid fallback kernels.";
    let first_body = "Pin cu128 on sm_120 to avoid fallback kernels.";
    insert(&conn, "rewrite", original);
    let (gateway, server) = fixture_gateway(vec![
        push_reply("rejected", first_body, 1),
        push_reply("created", first_body, 2),
    ]);
    let inf = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            keep_review(prompt)
        } else {
            first_body.into()
        }
    });
    let cache = std::env::temp_dir();
    let mut cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    cfg.gateway_config = Some(&gateway);
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().failed, 1);
    assert_eq!(
        q::get_memory_by_id(&conn, "rewrite").unwrap().content,
        original
    );
    assert!(q::get_memory_by_id(&conn, "rewrite")
        .unwrap()
        .condenser_version
        .is_none());
    assert!(q::get_memory_gateway_sync(&conn, "rewrite")
        .unwrap()
        .is_none());
    drop(conn);
    conn = open_database(&path).unwrap();
    let retried = run(&mut conn, &inf, &cfg).unwrap();
    assert_eq!(
        (retried.total_walked, retried.rewritten, retried.failed),
        (1, 1, 0)
    );
    assert_eq!(
        q::get_memory_by_id(&conn, "rewrite").unwrap().content,
        first_body
    );
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 0);
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn new_evidence_can_replace_processed_target_with_larger_combined_context() {
    for review_succeeded in [true, false] {
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
                if review_succeeded {
                    keep_review(prompt)
                } else {
                    "invalid review".into()
                }
            } else {
                "KEEP_UNCHANGED".into()
            }
        });
        let cache = std::env::temp_dir();
        let cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
        assert_eq!(
            run(&mut conn, &keep, &cfg).unwrap().failed,
            usize::from(!review_succeeded)
        );
        let untouched = (
            revision(&conn, "unrelated"),
            q::get_memory_by_id(&conn, "unrelated").unwrap().updated_at,
        );
        insert(&conn, "new", new);
        let merge = model(|prompt: &str| {
            assert!(prompt.contains(old) && prompt.contains(new));
            assert!(prompt.contains("pending memory ids:"));
            serde_json::json!({"decisions": {
                "old": {"action": "supersede_by", "content": combined},
                "new": {"action": "merge_into", "target_id": "old"},
                "unrelated": {"action": "keep"}
            }})
            .to_string()
        });
        let result = run(&mut conn, &merge, &cfg).unwrap();
        assert_eq!(result.total_walked, if review_succeeded { 1 } else { 3 });
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
        if review_succeeded {
            assert_eq!(
                untouched,
                (
                    revision(&conn, "unrelated"),
                    q::get_memory_by_id(&conn, "unrelated").unwrap().updated_at
                )
            );
        } else {
            assert_eq!(
                untouched.1,
                q::get_memory_by_id(&conn, "unrelated").unwrap().updated_at
            );
        }
        assert_eq!(run(&mut conn, &merge, &cfg).unwrap().total_walked, 0);
    }
}

#[test]
fn partial_processing_is_invalidated_by_edits_model_changes_and_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let mut conn = open_database(&path).unwrap();
    insert(
        &conn,
        "keep",
        "Supported guidance that a review must retry.",
    );
    let inf = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            "invalid review".into()
        } else {
            "KEEP_UNCHANGED".into()
        }
    });
    let cache = std::env::temp_dir();
    let mut cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().kept, 1);
    q::update_content(
        &conn,
        "keep",
        "Edited supported guidance that a review must retry.",
        None,
        None,
    )
    .unwrap();
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().kept, 1);
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().kept, 0); // review only
    cfg.model_name = "changed:model";
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().kept, 1);
    cfg.full = true;
    assert_eq!(run(&mut conn, &inf, &cfg).unwrap().kept, 1);
    assert_eq!(inf.prompts.lock().unwrap().len(), 9);
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
    for length in [15, 16] {
        let reply = serde_json::json!({"decisions": {
            "left": {"action": "merge_into", "target_id": "right"},
            "right": {"action": "supersede_by", "content": "é".repeat(length)}
        }})
        .to_string();
        let result = parse_response(&reply, &[left.clone(), right.clone()]);
        let decisions = result.unwrap();
        if length == 15 {
            assert!(matches!(
                decisions.get("left"),
                Some(Decision::MergeInto { .. })
            ));
            assert!(matches!(
                decisions.get("right"),
                Some(Decision::SupersedeBy { .. })
            ));
        } else {
            for id in ["left", "right"] {
                assert!(matches!(
                    decisions.get(id),
                    Some(Decision::KeepOriginal { .. })
                ));
            }
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
        .unwrap()
        .ends_with(":review-pending"));
    assert!(q::get_memory_by_id(&conn, "new")
        .unwrap()
        .condenser_version
        .is_some());
    conn.execute_batch("DROP TRIGGER block_old_delete").unwrap();
    assert_eq!(run(&mut conn, &drop, &cfg).unwrap().total_walked, 1);
    assert!(q::get_memory_by_id(&conn, "old").is_err());
    assert_eq!(run(&mut conn, &keep, &cfg).unwrap().total_walked, 0);
}

#[test]
fn over_budget_review_keeps_all_merge_sources_and_condensation_converges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let mut conn = open_database(&path).unwrap();
    let bodies = [
        ("source", "Supported source guidance."),
        ("target", "Supported target guidance."),
    ];
    for (id, body) in bodies {
        insert(&conn, id, body);
    }
    let inf = model(|prompt: &str| {
        if prompt.contains("Memory count:") {
            serde_json::json!({"decisions": {
                "source": {"action":"merge_into", "target_id":"target"},
                "target": {"action":"supersede_by", "content":"é".repeat(100)}
            }})
            .to_string()
        } else {
            "é".repeat(100)
        }
    });
    let cache = std::env::temp_dir();
    let mut cfg = DreamConfig::new(DreamMode::Dry, "test:model", &cache);
    let dry = run(&mut conn, &inf, &cfg).unwrap();
    assert_eq!((dry.failed, dry.kept, dry.review_kept), (0, 2, 2));
    for (id, body) in bodies {
        let m = q::get_memory_by_id(&conn, id).unwrap();
        assert_eq!(m.content, body);
        assert!(m.condenser_version.is_none());
        assert_eq!(revision(&conn, id), 1);
    }
    cfg.mode = DreamMode::Apply;
    let first = run(&mut conn, &inf, &cfg).unwrap();
    assert_eq!((first.failed, first.kept, first.review_kept), (0, 2, 2));
    assert_eq!(
        q::list_dream_candidates(&conn, Some("test"), None, None, 0)
            .unwrap()
            .len(),
        2
    );
    let calls = inf.prompts.lock().unwrap().len();
    for _ in 0..2 {
        drop(conn);
        conn = open_database(&path).unwrap();
        assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 0);
        for (id, body) in bodies {
            let m = q::get_memory_by_id(&conn, id).unwrap();
            assert_eq!(m.content, body);
            assert!(m.content_raw.is_none());
            assert!(m.superseded_by.is_none());
            assert!(m.condenser_version.is_some());
        }
    }
    assert_eq!(inf.prompts.lock().unwrap().len(), calls);
}

#[test]
fn persistent_gateway_conflict_rolls_back_rewrite_and_converges_but_rejection_retries() {
    use agent_memory::db::models::MemoryGatewaySyncUpsert;
    for action in ["conflict", "rejected"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let mut conn = open_database(&path).unwrap();
        let original = "Pin cu128 explicitly on sm_120 to avoid fallback kernels.";
        let rewritten = "Pin cu128 on sm_120 to avoid fallback kernels.";
        insert(&conn, "rewrite", original);
        q::upsert_memory_gateway_sync(
            &conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: "rewrite".into(),
                project: "test".into(),
                gateway_memory_id: "gw-rewrite".into(),
                last_seen_server_revision: 5,
                last_pushed_content_hash: Some(agent_memory::sync::memory_content_hash(
                    original,
                    "user",
                    &[],
                )),
                last_pulled_content_hash: None,
                sync_state: "linked".into(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )
        .unwrap();
        let mut responses = vec![
            serde_json::json!({"project_ident":"test", "server_revision":6, "results":[{
                "local_memory_id":"rewrite", "gateway_memory_id":"gw-rewrite", "server_revision":6,
                "action":action, "error":"fixture policy or revision conflict"
            }]}),
        ];
        if action == "conflict" {
            responses.push(serde_json::json!({"project_ident":"test", "server_revision":6, "memories":[{
                "project_ident":"test", "gateway_memory_id":"gw-rewrite", "server_revision":6,
                "content":"independent remote edit", "memory_type":"user",
                "content_hash":agent_memory::sync::memory_content_hash("independent remote edit", "user", &[])
            }]}));
        }
        let (gateway, server) = fixture_gateway(responses);
        let inf = model(|prompt: &str| {
            if prompt.contains("Memory count:") {
                keep_review(prompt)
            } else {
                rewritten.into()
            }
        });
        let cache = std::env::temp_dir();
        let mut cfg = DreamConfig::new(DreamMode::Apply, "test:model", &cache);
        cfg.gateway_config = Some(&gateway);
        let result = run(&mut conn, &inf, &cfg).unwrap();
        assert_eq!(result.rewritten, 0);
        assert_eq!(result.failed, usize::from(action == "rejected"));
        assert_eq!(result.kept, usize::from(action == "conflict"));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), if action == "conflict" { 2 } else { 1 });
        let m = q::get_memory_by_id(&conn, "rewrite").unwrap();
        assert_eq!(m.content, original);
        assert!(m.content_raw.is_none());
        assert_eq!(m.condenser_version.is_some(), action == "conflict");
        let rewrite_revisions: i64 = conn
            .query_row(
                "SELECT count(*) FROM memory_revisions WHERE operation = 'dream_condense'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rewrite_revisions, 0);
        assert_eq!(
            q::get_memory_gateway_sync(&conn, "rewrite")
                .unwrap()
                .unwrap()
                .last_seen_server_revision,
            5
        );
        if action == "conflict" {
            let calls = inf.prompts.lock().unwrap().len();
            for _ in 0..2 {
                drop(conn);
                conn = open_database(&path).unwrap();
                assert_eq!(run(&mut conn, &inf, &cfg).unwrap().total_walked, 0);
            }
            assert_eq!(inf.prompts.lock().unwrap().len(), calls);
        }
    }
}
