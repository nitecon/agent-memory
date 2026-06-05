//! Project identification from the current working directory.
//!
//! Returns the repository shortname (e.g. `eventic` for
//! `git@github.com:nitecon/eventic.git`) so that memories auto-tagged by the
//! cwd resolver share an ident with the logical, human-written project labels
//! most agents already use (`rithmic`, `traderx`, `agent-tools`, ...). For
//! non-git directories, falls back to the directory basename. Git-derived and
//! fallback idents are lowercased to match the gateway/project-board
//! convention and avoid Windows path-case drift.
//!
//! Trade-off: two repos with the same basename across different orgs will
//! collide on ident. This is intentional -- the alternative (full host/org/repo
//! path) makes cwd-derived idents look foreign to corpora written by hand.
//!
//! Used by the memory system to (a) auto-tag stored memories with the current
//! project and (b) boost the relevance of current-project memories at
//! search/context time while still surfacing cross-project results.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Resolve a stable project identifier from a project root path.
pub fn project_ident(project_root: &Path) -> String {
    if let Some(url) = git_remote_url_from_metadata(project_root) {
        return normalize_git_url(&url);
    }

    if let Ok(output) = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(project_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    {
        if output.status.success() {
            let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !url.is_empty() {
                return normalize_git_url(&url);
            }
        }
    }

    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| canonical.to_string_lossy().to_ascii_lowercase())
}

/// Resolve the project ident for the current working directory.
pub fn project_ident_from_cwd() -> std::io::Result<String> {
    let cwd = std::env::current_dir()?;
    Ok(project_ident(&cwd))
}

/// Extract the repository shortname from a git remote URL.
///
/// Handles HTTPS, SSH-explicit (`ssh://git@...`), and SSH-shorthand
/// (`git@host:org/repo.git`) forms. Returns the final path segment with
/// any `.git` suffix stripped and lowercased -- e.g. `agent-memory` for
/// `https://github.com/nitecon/agent-memory.git`.
fn normalize_git_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    // Split on both `/` and `:` so SSH shorthand (`host:org/repo`) and HTTPS
    // paths yield the same final segment.
    let last = trimmed
        .rsplit(['/', ':'])
        .find(|seg| !seg.is_empty())
        .unwrap_or(trimmed);
    last.strip_suffix(".git")
        .unwrap_or(last)
        .to_ascii_lowercase()
}

#[derive(Debug, PartialEq, Eq)]
struct GitMetadata {
    git_dir: PathBuf,
    common_dir: PathBuf,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct ParsedGitConfig {
    remotes: HashMap<String, String>,
    branch_remotes: HashMap<String, String>,
    remote_order: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum GitConfigSection {
    Remote(String),
    Branch(String),
    Other,
}

fn git_remote_url_from_metadata(project_root: &Path) -> Option<String> {
    let metadata = find_git_metadata(project_root)?;
    let config = parse_git_config(&metadata.common_dir.join("config"))?;
    let current_branch = read_current_branch(&metadata.git_dir);
    choose_remote_url(&config, current_branch.as_deref())
}

fn find_git_metadata(project_root: &Path) -> Option<GitMetadata> {
    let mut current = Some(project_root);
    while let Some(dir) = current {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(GitMetadata {
                git_dir: dot_git.clone(),
                common_dir: dot_git,
            });
        }
        if dot_git.is_file() {
            let git_dir = parse_gitdir_file(&dot_git)?;
            let common_dir = resolve_common_dir(&git_dir);
            return Some(GitMetadata {
                git_dir,
                common_dir,
            });
        }
        current = dir.parent();
    }
    None
}

fn parse_gitdir_file(dot_git: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(dot_git).ok()?;
    let path = content.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = PathBuf::from(path);
    if git_dir.is_absolute() {
        Some(git_dir)
    } else {
        Some(dot_git.parent()?.join(git_dir))
    }
}

fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    let commondir = git_dir.join("commondir");
    let Ok(content) = std::fs::read_to_string(&commondir) else {
        return git_dir.to_path_buf();
    };
    let path = PathBuf::from(content.trim());
    if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    }
}

fn read_current_branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let reference = head.trim().strip_prefix("ref:")?.trim();
    reference.strip_prefix("refs/heads/").map(str::to_string)
}

fn parse_git_config(path: &Path) -> Option<ParsedGitConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut parsed = ParsedGitConfig::default();
    let mut section = GitConfigSection::Other;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = parse_git_config_section(trimmed);
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();

        match &section {
            GitConfigSection::Remote(name) if key == "url" && !value.is_empty() => {
                if !parsed.remotes.contains_key(name) {
                    parsed.remote_order.push(name.clone());
                }
                parsed.remotes.insert(name.clone(), value);
            }
            GitConfigSection::Branch(name) if key == "remote" && !value.is_empty() => {
                parsed.branch_remotes.insert(name.clone(), value);
            }
            _ => {}
        }
    }

    Some(parsed)
}

fn parse_git_config_section(section: &str) -> GitConfigSection {
    let body = section
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if let Some(name) = parse_named_git_section(body, "remote") {
        return GitConfigSection::Remote(name);
    }
    if let Some(name) = parse_named_git_section(body, "branch") {
        return GitConfigSection::Branch(name);
    }
    GitConfigSection::Other
}

fn parse_named_git_section(body: &str, kind: &str) -> Option<String> {
    let rest = body.strip_prefix(kind)?.trim_start();
    let quoted = rest.strip_prefix('"')?;
    let end = quoted.rfind('"')?;
    Some(quoted[..end].to_string())
}

fn choose_remote_url(config: &ParsedGitConfig, current_branch: Option<&str>) -> Option<String> {
    if let Some(branch) = current_branch {
        if let Some(remote_name) = config.branch_remotes.get(branch) {
            if remote_name != "." {
                if let Some(url) = config.remotes.get(remote_name) {
                    return Some(url.clone());
                }
            }
        }
    }
    if let Some(url) = config.remotes.get("origin") {
        return Some(url.clone());
    }
    config
        .remote_order
        .iter()
        .find_map(|remote| config.remotes.get(remote))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("agent-memory-project-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }

    fn write_git_config(root: &Path, config: &str) {
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).expect("create .git");
        fs::write(git_dir.join("config"), config).expect("write config");
    }

    #[test]
    fn normalize_ssh_shorthand() {
        assert_eq!(
            normalize_git_url("git@github.com:nitecon/agent-memory.git"),
            "agent-memory"
        );
    }

    #[test]
    fn normalize_https() {
        assert_eq!(
            normalize_git_url("https://github.com/nitecon/agent-memory.git"),
            "agent-memory"
        );
    }

    #[test]
    fn normalize_uppercase_repo_name_to_gateway_ident() {
        assert_eq!(normalize_git_url("git@github.com:nitecon/X.git"), "x");
        assert_eq!(normalize_git_url("https://github.com/nitecon/X.git"), "x");
    }

    #[test]
    fn normalize_ssh_explicit() {
        assert_eq!(
            normalize_git_url("ssh://git@github.com/nitecon/agent-memory.git"),
            "agent-memory"
        );
    }

    #[test]
    fn normalize_https_with_user() {
        assert_eq!(
            normalize_git_url("https://user@github.com/nitecon/agent-memory.git"),
            "agent-memory"
        );
    }

    #[test]
    fn ssh_and_https_match() {
        assert_eq!(
            normalize_git_url("git@github.com:nitecon/agent-memory.git"),
            normalize_git_url("https://github.com/nitecon/agent-memory.git"),
        );
    }

    #[test]
    fn eventic_shortname() {
        assert_eq!(
            normalize_git_url("git@github.com:nitecon/eventic.git"),
            "eventic"
        );
        assert_eq!(
            normalize_git_url("https://github.com/nitecon/eventic.git"),
            "eventic"
        );
    }

    #[test]
    fn no_dot_git_suffix() {
        assert_eq!(
            normalize_git_url("https://github.com/nitecon/eventic"),
            "eventic"
        );
    }

    #[test]
    fn trailing_slash_is_ignored() {
        assert_eq!(
            normalize_git_url("https://github.com/nitecon/eventic.git/"),
            "eventic"
        );
    }

    #[test]
    fn gitlab_nested_group_uses_final_segment() {
        assert_eq!(
            normalize_git_url("https://gitlab.com/group/subgroup/my-repo.git"),
            "my-repo"
        );
    }

    #[test]
    fn project_ident_lowercases_directory_fallback() {
        let root = temp_root();
        let project_root = root.join("X");
        fs::create_dir_all(&project_root).expect("create project");

        assert_eq!(project_ident(&project_root), "x");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_ident_reads_git_config_before_uppercase_directory_fallback() {
        let root = temp_root();
        let project_root = root.join("X");
        fs::create_dir_all(&project_root).expect("create project");
        write_git_config(
            &project_root,
            r#"
[remote "origin"]
    url = https://github.com/nitecon/x.git
"#,
        );

        assert_eq!(project_ident(&project_root), "x");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_ident_lowercases_uppercase_origin_repo_name() {
        let root = temp_root();
        let project_root = root.join("X");
        fs::create_dir_all(&project_root).expect("create project");
        write_git_config(
            &project_root,
            r#"
[remote "origin"]
    url = https://github.com/nitecon/X.git
"#,
        );

        assert_eq!(project_ident(&project_root), "x");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_ident_prefers_current_branch_upstream_remote() {
        let root = temp_root();
        let project_root = root.join("X");
        fs::create_dir_all(&project_root).expect("create project");
        write_git_config(
            &project_root,
            r#"
[remote "origin"]
    url = https://github.com/nitecon/X.git
[remote "upstream"]
    url = https://github.com/nitecon/x.git
[branch "main"]
    remote = upstream
    merge = refs/heads/main
"#,
        );
        fs::write(
            project_root.join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("write HEAD");

        assert_eq!(project_ident(&project_root), "x");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_ident_follows_gitdir_file_and_commondir() {
        let root = temp_root();
        let project_root = root.join("X");
        let common_dir = root.join("repo").join(".git");
        let git_dir = common_dir.join("worktrees").join("X");
        fs::create_dir_all(&project_root).expect("create project");
        fs::create_dir_all(&git_dir).expect("create worktree git dir");
        fs::write(
            project_root.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .expect("write .git file");
        fs::write(git_dir.join("commondir"), "../..\n").expect("write commondir");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
        fs::write(
            common_dir.join("config"),
            r#"
[remote "origin"]
    url = https://github.com/nitecon/x.git
[branch "main"]
    remote = origin
"#,
        )
        .expect("write common config");

        assert_eq!(project_ident(&project_root), "x");
        let _ = fs::remove_dir_all(root);
    }
}
