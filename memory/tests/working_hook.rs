use std::path::Path;
use std::process::{Command, Stdio};

fn memory(data: &Path, project: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_memory"))
        .args(args)
        .current_dir(project)
        .env("AGENT_MEMORY_DIR", data)
        .env("AGENT_MEMORY_NO_UPDATE", "1")
        .env("AGENT_MEMORY_GATEWAY_AUTO_SYNC", "off")
        .env("AGENT_MEMORY_HOOK", "on")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn working_get_and_hooks_preserve_original_context_and_all_appends() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let project = root.path().join("handoff-project");
    std::fs::create_dir(&project).unwrap();
    memory(
        &data,
        &project,
        &["working", "set", "Original task context"],
    );
    for note in ["First follow-up decision", "Second follow-up next step"] {
        memory(&data, &project, &["working", "append", note]);
    }

    let get = memory(&data, &project, &["working", "get"]);
    let expected = "Original task context\n\n## Ammendment 1\nFirst follow-up decision\n\n## Ammendment 2\nSecond follow-up next step";
    assert!(get.contains(expected), "{get}");
    assert!(get.contains("version=\"3\""), "{get}");
    for agent in ["claude", "codex", "gemini"] {
        let hook = memory(&data, &project, &["hook", "--agent", agent]);
        let json: serde_json::Value = serde_json::from_str(&hook).unwrap();
        let context = json["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.starts_with(get.trim_end()), "{context}");
        let hint = context.strip_prefix(get.trim_end()).unwrap().trim();
        assert!(hint.starts_with("<hint>"), "{hint}");
        assert!(hint.contains("memory working append"), "{hint}");
        assert!(hint.contains("future agents continuing the current task"));
        assert!(hint.ends_with("</hint>"), "{hint}");
    }
}
