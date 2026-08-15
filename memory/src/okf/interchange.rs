use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use rusqlite::{params, Connection};

use super::{BundleScope, HandlerError, OkfBundleHandler, OkfDocumentHandler};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportResult {
    pub files: Vec<PathBuf>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportResult {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub dry_run: bool,
}

pub fn export_bundle(
    conn: &Connection,
    scope: BundleScope,
    target: &Path,
    ids: &[String],
    dry_run: bool,
) -> Result<ExportResult, HandlerError> {
    validate_target_path(target)?;
    let bundle = OkfBundleHandler::new(conn, scope.clone());
    let document = OkfDocumentHandler::new(conn, scope);
    let selected = if ids.is_empty() {
        bundle
            .list("/memories")?
            .into_iter()
            .filter_map(|entry| {
                entry
                    .path
                    .strip_prefix("/memories/")
                    .and_then(|name| name.strip_suffix(".md"))
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
    } else {
        let mut selected = ids.to_vec();
        selected.sort();
        selected.dedup();
        selected
    };

    let mut files = Vec::new();
    let mut payloads = Vec::new();
    let mut root_index = format!("# Exported memory bundle\n\nBundle: `{}`\n\n", bundle.uri());
    for id in selected {
        let rendered = document.render(&id)?;
        reject_secret_markers(&rendered.text, "export")?;
        let relative = PathBuf::from("memories").join(format!("{}.md", rendered.id));
        root_index.push_str(&format!(
            "- [`{}`](memories/{}.md)\n",
            rendered.id, rendered.id
        ));
        payloads.push((relative, rendered.text));
    }
    reject_secret_markers(&root_index, "export")?;
    payloads.push((PathBuf::from("index.md"), root_index));
    payloads.push((
        PathBuf::from("log.md"),
        bundle.log(None, Some(200))?.document.content,
    ));
    payloads.sort_by(|left, right| left.0.cmp(&right.0));
    for (_, content) in &payloads {
        reject_secret_markers(content, "export")?;
    }

    for (relative, _) in &payloads {
        let path = target.join(relative);
        validate_target_path(&path)?;
        if path.exists() {
            return Err(HandlerError::InvalidTarget(format!(
                "export destination already exists: {}",
                path.display()
            )));
        }
        files.push(path);
    }
    if dry_run {
        return Ok(ExportResult {
            files,
            dry_run: true,
        });
    }
    for ((_, content), path) in payloads.into_iter().zip(&files) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(crate::error::MemoryError::from)?;
            reject_symlink_components(parent)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(crate::error::MemoryError::from)?;
        file.write_all(content.as_bytes())
            .map_err(crate::error::MemoryError::from)?;
    }
    Ok(ExportResult {
        files,
        dry_run: false,
    })
}

pub fn import_bundle(
    conn: &Connection,
    scope: BundleScope,
    source: &Path,
    dry_run: bool,
) -> Result<ImportResult, HandlerError> {
    reject_symlink_components(source)?;
    let files = import_files(source)?;
    let handler = OkfDocumentHandler::new(conn, scope);
    let mut result = ImportResult {
        created: 0,
        updated: 0,
        unchanged: 0,
        dry_run,
    };
    for file in files {
        reject_symlink_components(&file)?;
        let text = fs::read_to_string(&file).map_err(crate::error::MemoryError::from)?;
        let parsed = handler.parse(&text)?;
        let filename_id = file
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| *value != "index" && *value != "log");
        let metadata_id = parsed
            .concept
            .agent_memory
            .as_ref()
            .and_then(|metadata| metadata.id.as_deref());
        let target = metadata_id.or(filename_id);
        let exists = target
            .map(|id| {
                conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
                    params![id],
                    |row| row.get::<_, bool>(0),
                )
            })
            .transpose()?
            .unwrap_or(false);
        let expected = if exists {
            parsed
                .concept
                .agent_memory
                .as_ref()
                .and_then(|metadata| metadata.revision)
                .and_then(|revision| i64::try_from(revision).ok())
        } else {
            None
        };
        let put = handler.put_with_operation(target, &parsed, expected, dry_run, "import")?;
        if put.created {
            result.created += 1;
        } else if put.changed {
            result.updated += 1;
        } else {
            result.unchanged += 1;
        }
    }
    Ok(result)
}

fn import_files(source: &Path) -> Result<Vec<PathBuf>, HandlerError> {
    if source.is_file() {
        return Ok(vec![source.to_path_buf()]);
    }
    let memories = source.join("memories");
    if !memories.is_dir() {
        return Err(HandlerError::InvalidTarget(format!(
            "import source has no memories directory: {}",
            source.display()
        )));
    }
    reject_symlink_components(&memories)?;
    let mut files = fs::read_dir(&memories)
        .map_err(crate::error::MemoryError::from)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(crate::error::MemoryError::from)?;
    files.retain(|path| path.extension().and_then(|value| value.to_str()) == Some("md"));
    files.sort();
    Ok(files)
}

fn validate_target_path(path: &Path) -> Result<(), HandlerError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(HandlerError::InvalidTarget(path.display().to_string()));
    }
    reject_symlink_components(path)
}

fn reject_symlink_components(path: &Path) -> Result<(), HandlerError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(HandlerError::InvalidTarget(format!(
                    "symlink paths are not allowed: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(crate::error::MemoryError::from(error).into()),
        }
    }
    Ok(())
}

pub fn reject_secret_markers(text: &str, operation: &str) -> Result<(), HandlerError> {
    let upper = text.to_ascii_uppercase();
    let suspicious = [
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN=",
        "API_KEY=",
        "PASSWORD=",
    ];
    if let Some(marker) = suspicious.iter().find(|marker| upper.contains(**marker)) {
        return Err(HandlerError::InvalidTarget(format!(
            "{operation} blocked by secret marker `{marker}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{models::Memory, open_database, queries};

    fn insert(conn: &Connection, id: &str, body: &str) {
        let mut memory = Memory::new(
            body.to_string(),
            Some(vec!["export".to_string()]),
            Some("alpha".to_string()),
            None,
            None,
            Some("reference".to_string()),
        );
        memory.id = id.to_string();
        queries::insert_memory(conn, &memory).expect("insert");
    }

    #[test]
    fn export_dry_run_writes_nothing_and_round_trip_is_deterministic() {
        let source = open_database(Path::new(":memory:")).expect("source database");
        insert(&source, "aaaaaaaa-export", "portable body");
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("bundle");
        let dry = export_bundle(
            &source,
            BundleScope::Project("alpha".to_string()),
            &target,
            &[],
            true,
        )
        .expect("dry export");
        assert_eq!(dry.files.len(), 3);
        assert!(!target.exists());

        export_bundle(
            &source,
            BundleScope::Project("alpha".to_string()),
            &target,
            &[],
            false,
        )
        .expect("export");
        let destination = open_database(Path::new(":memory:")).expect("destination database");
        let imported = import_bundle(
            &destination,
            BundleScope::Project("alpha".to_string()),
            &target,
            false,
        )
        .expect("import");
        assert_eq!(imported.created, 1);
        assert_eq!(
            queries::get_memory_by_id(&destination, "aaaaaaaa-export")
                .expect("imported memory")
                .content,
            "portable body"
        );
        let second = import_bundle(
            &destination,
            BundleScope::Project("alpha".to_string()),
            &target,
            true,
        )
        .expect("dry reimport");
        assert_eq!(second.unchanged, 1);
    }

    #[test]
    fn export_blocks_secrets_and_existing_destinations_before_writing() {
        let conn = open_database(Path::new(":memory:")).expect("database");
        insert(&conn, "aaaaaaaa-secret", "API_KEY=do-not-export");
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("bundle");
        assert!(export_bundle(
            &conn,
            BundleScope::Project("alpha".to_string()),
            &target,
            &[],
            false,
        )
        .is_err());
        assert!(!target.exists());

        let safe = open_database(Path::new(":memory:")).expect("safe database");
        insert(&safe, "aaaaaaaa-safe", "safe");
        fs::create_dir_all(&target).expect("target");
        fs::write(target.join("index.md"), "owned").expect("existing file");
        assert!(export_bundle(
            &safe,
            BundleScope::Project("alpha".to_string()),
            &target,
            &[],
            false,
        )
        .is_err());
        assert_eq!(
            fs::read_to_string(target.join("index.md")).unwrap(),
            "owned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn import_and_export_reject_symlink_traversal() {
        use std::os::unix::fs::symlink;

        let conn = open_database(Path::new(":memory:")).expect("database");
        insert(&conn, "aaaaaaaa-link", "safe");
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).expect("outside");
        let linked = temp.path().join("linked");
        symlink(&outside, &linked).expect("symlink");
        assert!(export_bundle(
            &conn,
            BundleScope::Project("alpha".to_string()),
            &linked,
            &[],
            false,
        )
        .is_err());
        assert!(import_bundle(
            &conn,
            BundleScope::Project("alpha".to_string()),
            &linked,
            true,
        )
        .is_err());
    }
}
