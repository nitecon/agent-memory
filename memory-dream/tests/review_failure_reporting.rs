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

#[cfg(unix)]
#[test]
fn policy_convergence_exits_zero_and_real_failures_match_structured_records() {
    use agent_memory::db::{models::Memory, open_database, queries};
    use std::process::Command;

    for failure in ["none", "malformed", "stamp"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        let conn = open_database(&path).unwrap();
        let ids = [
            "82fdd9d6", "4287507a", "6cc9e52b", "c439b1b4", "a48a23ca", "9071f66d", "0ed4b8e9",
            "f5b09445",
        ];
        for id in ids
            .into_iter()
            .chain((failure == "malformed").then_some("bad"))
        {
            let mut m = Memory::new(
                format!("Supported guidance for {id}."),
                None,
                Some("fixture".into()),
                None,
                None,
                Some("user".into()),
            );
            m.id = id.into();
            queries::insert_memory(&conn, &m).unwrap();
        }
        if failure == "stamp" {
            conn.execute_batch("CREATE TRIGGER fail_stamp BEFORE UPDATE OF condenser_version ON memories WHEN NEW.id='82fdd9d6' BEGIN SELECT RAISE(ABORT, 'injected stamp failure'); END;").unwrap();
        }
        drop(conn);
        let script = r#"import sys,json; p=sys.stdin.read(); ids=[s[4:] for s in p.splitlines() if s.startswith("id: ")]; print(json.dumps({"decisions":{i:({"action":"extract","content":"é"*2000} if i=="f5b09445" else {"action":"keep"}) for i in ids}}) if "Memory count:" in p else ("KEEP_UNCHANGED malformed" if "Supported guidance for bad." in p else "é"*2000))"#;
        let command = format!("python3 -c '{script}' {{prompt_stdin}}");
        let invoke = || {
            Command::new(env!("CARGO_BIN_EXE_memory-dream"))
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
                .unwrap()
        };
        for pass in 0..2 {
            let out = invoke();
            let stdout = String::from_utf8(out.stdout).unwrap();
            assert_eq!(out.status.success(), failure == "none", "{stdout}");
            let failures = stdout
                .lines()
                .filter(|line| {
                    [
                        "review_failed",
                        "review_apply_failed",
                        "condense_failed",
                        "processing_failed",
                        "condense_gateway_sync_failed",
                        "dedup_failed",
                    ]
                    .iter()
                    .any(|status| line.contains(&format!("status=\"{status}\"")))
                })
                .count();
            let summary = stdout
                .lines()
                .find(|line| line.contains("status=\"dream_complete\""))
                .unwrap();
            assert!(
                summary.contains(&format!("failed=\"{failures}\"")),
                "{stdout}"
            );
            if failure == "none" {
                assert_eq!(failures, 0);
                if pass == 0 {
                    for id in ids {
                        assert!(
                            stdout
                                .lines()
                                .any(|line| line.contains("status=\"condense_converged\"")
                                    && line.contains(&format!("id=\"{id}\""))),
                            "{stdout}"
                        );
                    }
                    assert!(
                        stdout.contains("status=\"review_converged\" id=\"f5b09445\""),
                        "{stdout}"
                    );
                } else {
                    assert!(summary.contains("walked=\"0\""), "{stdout}");
                }
            } else {
                assert!(failures > 0, "{stdout}");
                if pass == 1 {
                    assert!(summary.contains("walked=\"1\""), "{stdout}");
                }
            }
            let conn = open_database(&path).unwrap();
            for id in ids {
                let m = queries::get_memory_by_id(&conn, id).unwrap();
                assert_eq!(m.content, format!("Supported guidance for {id}."));
                assert!(m.superseded_by.is_none());
                assert!(m.content_raw.is_none());
            }
        }
    }
}
