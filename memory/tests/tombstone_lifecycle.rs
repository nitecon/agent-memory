use agent_memory::db::{models::Memory, open_database, queries};
use agent_memory::okf::{BundleScope, OkfDocumentHandler};
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn invoke(data: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_memory"))
        .args(args)
        .current_dir(cwd)
        .env("AGENT_MEMORY_DIR", data)
        .env("AGENT_MEMORY_NO_UPDATE", "1")
        .env("AGENT_MEMORY_GATEWAY_AUTO_SYNC", "false")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn cli_reports_blocked_update_and_copy_recovery_without_losing_local_guidance() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let cwd = root.path().join("unrealagent");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    let project = agent_memory::project::project_ident(&cwd);
    let conn = open_database(&data.join("memory.db")).unwrap();
    let mut memory = Memory::new(
        "undo transactions guidance saved by old client".into(),
        Some(vec!["undo".into()]),
        Some(project.clone()),
        None,
        None,
        Some("project".into()),
    );
    memory.id = "6dbc09dc-0000-1111-2222-000000000001".into();
    queries::insert_memory(&conn, &memory).unwrap();
    queries::upsert_memory_gateway_sync(
        &conn,
        &agent_memory::db::models::MemoryGatewaySyncUpsert {
            local_memory_id: memory.id.clone(),
            project: project.clone(),
            gateway_memory_id: "gw-deleted".into(),
            last_seen_server_revision: 10,
            last_pushed_content_hash: Some("old-hash".into()),
            last_pulled_content_hash: None,
            sync_state: "tombstone".into(),
            tombstone_deleted: true,
            tombstone_at: None,
        },
    )
    .unwrap();

    // Amux memory_update invokes this CLI form. It must see a failing exit,
    // actionable recovery and no false local-success result.
    let output = invoke(
        &data,
        &cwd,
        &["update", "6dbc09dc", "--content", "new text"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("memory copy --id"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("status=\"updated\""));
    assert_eq!(
        queries::get_memory_by_id(&conn, &memory.id)
            .unwrap()
            .content,
        memory.content
    );

    let status = invoke(&data, &cwd, &["push", "status"]);
    assert!(status.status.success());
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(status.contains("action=\"blocked_tombstone\""), "{status}");
    assert!(status.contains("blocked=\"1\""), "{status}");
    assert!(status.contains("skipped=\"0\""), "{status}");

    // The OKF replacement path shares the same guard and immutable history.
    let handler = OkfDocumentHandler::new(&conn, BundleScope::Project(project.clone()));
    let rendered = handler.render(&memory.id).unwrap();
    let parsed = handler
        .parse(&rendered.text.replace(&memory.content, "OKF edit"))
        .unwrap();
    let error = handler
        .put(Some(&memory.id), &parsed, None, false)
        .unwrap_err();
    assert!(error.to_string().contains("tombstoned gateway identity"));
    assert_eq!(
        agent_memory::concepts::current_revision(&conn, &memory.id).unwrap(),
        1
    );

    let copied = invoke(&data, &cwd, &["copy", "--id", "6dbc09dc", "--to", &project]);
    assert!(copied.status.success(), "{copied:?}");
    let copied_output = String::from_utf8(copied.stdout).unwrap();
    assert!(copied_output.contains("new_id=\""), "{copied_output}");
    let memories = queries::list_memories_by_project(&conn, Some(&project)).unwrap();
    assert_eq!(memories.len(), 2);
    let copy = memories.iter().find(|m| m.id != memory.id).unwrap();
    assert_eq!(copy.content, memory.content);
    assert!(queries::get_memory_gateway_sync(&conn, &copy.id)
        .unwrap()
        .is_none());
    assert!(
        queries::get_memory_gateway_sync(&conn, &memory.id)
            .unwrap()
            .unwrap()
            .tombstone_deleted
    );
    assert_eq!(
        agent_memory::concepts::current_revision(&conn, &memory.id).unwrap(),
        1
    );
}
