//! Exercise failure accounting through the real CLI without model usage.
#[cfg(unix)]
#[test]
fn counted_merge_and_processing_failures_have_structured_records_and_keep_sources() {
    use agent_memory::db::{models::Memory, open_database, queries};
    use std::process::Command;

    for fail_processing_stamp in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_database(&dir.path().join("memory.db")).unwrap();
        for (id, body) in [
            (
                "source",
                "Prefer tabs because indentation width is configurable.",
            ),
            (
                "target",
                "Pin cu128 on sm_120 to avoid silent kernel fallback.",
            ),
        ] {
            let mut memory = Memory::new(
                body.into(),
                None,
                Some("fixture".into()),
                None,
                None,
                Some("user".into()),
            );
            memory.id = id.into();
            queries::insert_memory(&conn, &memory).unwrap();
        }
        conn.execute_batch("CREATE TRIGGER fail_merge BEFORE UPDATE OF superseded_by ON memories WHEN NEW.id='source' BEGIN SELECT RAISE(ABORT, 'injected merge failure'); END;").unwrap();
        if fail_processing_stamp {
            conn.execute_batch("CREATE TRIGGER fail_stamp BEFORE UPDATE OF condenser_version ON memories BEGIN SELECT RAISE(ABORT, 'injected processing failure'); END;").unwrap();
        }
        drop(conn);

        let review = r#"{"decisions":{"source":{"action":"merge_into","target_id":"target"},"target":{"action":"keep"}}}"#;
        let script = format!(
            "import sys; p=sys.stdin.read(); print({} if \"Memory count:\" in p else \"KEEP_UNCHANGED\")",
            serde_json::to_string(review).unwrap(),
        );
        // The command template is tokenized directly, without a shell. All
        // fixture output travels through stdin/stdout; no script file is needed.
        let command = format!("python3 -c '{script}' {{prompt_stdin}}");
        let out = Command::new(env!("CARGO_BIN_EXE_memory-dream"))
            .args([
                "--backend",
                "headless",
                "--headless-model",
                "fixture",
                "--command-override",
                &command,
            ])
            .env("AGENT_MEMORY_DIR", dir.path())
            .env("AGENT_MEMORY_NO_UPDATE", "1")
            .env("AGENT_MEMORY_GATEWAY_AUTO_SYNC", "false")
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(!out.status.success(), "{stdout}");
        let records: Vec<_> = stdout
            .lines()
            .filter(|line| {
                line.contains("status=\"review_apply_failed\"")
                    || line.contains("status=\"processing_failed\"")
            })
            .collect();
        assert_eq!(
            records.len(),
            if fail_processing_stamp { 3 } else { 1 },
            "{stdout}"
        );
        let summary = stdout
            .lines()
            .find(|line| line.contains("status=\"dream_complete\""))
            .unwrap();
        assert!(
            summary.contains(&format!("failed=\"{}\"", records.len())),
            "{stdout}"
        );
        assert!(records
            .iter()
            .any(|line| line.contains("injected merge failure")));
        let conn = open_database(&dir.path().join("memory.db")).unwrap();
        let source = queries::get_memory_by_id(&conn, "source").unwrap();
        assert!(source.superseded_by.is_none());
        assert_eq!(
            source.content,
            "Prefer tabs because indentation width is configurable."
        );
    }
}
