use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::error::MemoryError;

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub model_cache_dir: PathBuf,
    pub gateway: GatewayConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

impl GatewayConfig {
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var("AGENT_MEMORY_GATEWAY_URL").ok(),
            std::env::var("AGENT_GATEWAY_URL").ok(),
            std::env::var("AGENT_MEMORY_GATEWAY_API_KEY").ok(),
            std::env::var("AGENT_GATEWAY_API_KEY").ok(),
        )
    }

    fn from_values(
        memory_url: Option<String>,
        gateway_url: Option<String>,
        memory_api_key: Option<String>,
        gateway_api_key: Option<String>,
    ) -> Self {
        Self {
            base_url: first_nonempty(memory_url, gateway_url),
            api_key: first_nonempty(memory_api_key, gateway_api_key),
        }
    }
}

fn first_nonempty(primary: Option<String>, fallback: Option<String>) -> Option<String> {
    primary
        .and_then(nonempty_trimmed)
        .or_else(|| fallback.and_then(nonempty_trimmed))
}

fn nonempty_trimmed(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

impl Config {
    /// Resolve the data directory with the following priority:
    ///
    /// 1. `AGENT_MEMORY_DIR` environment variable (explicit override)
    /// 2. `~/.agentic/` if `~/.agentic/memory.db` already exists (user-local)
    /// 3. `/opt/agentic/` if `/opt/agentic/memory.db` already exists and is writable
    ///    (legacy shared DB)
    /// 4. `~/.agentic/` as the default
    pub fn load() -> Result<Self, MemoryError> {
        let data_dir = Self::resolve_data_dir(
            std::env::var_os("AGENT_MEMORY_DIR"),
            Self::user_local_dir(),
            Self::global_dir(),
        )?;

        Ok(Self {
            db_path: data_dir.join("memory.db"),
            model_cache_dir: data_dir.join("models"),
            gateway: GatewayConfig::from_env(),
            data_dir,
        })
    }

    /// User-local directory: ~/.agentic/
    fn user_local_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".agentic"))
    }

    /// Global directory: /opt/agentic/ on Unix, %USERPROFILE%\.agentic\ on Windows
    fn global_dir() -> Option<PathBuf> {
        if cfg!(windows) {
            dirs::home_dir().map(|h| h.join(".agentic"))
        } else {
            Some(PathBuf::from("/opt/agentic"))
        }
    }

    fn resolve_data_dir(
        env_dir: Option<OsString>,
        user_dir: Option<PathBuf>,
        global_dir: Option<PathBuf>,
    ) -> Result<PathBuf, MemoryError> {
        if let Some(dir) = env_dir {
            return Ok(PathBuf::from(dir));
        }

        if let Some(user_dir) = user_dir.as_ref() {
            if user_dir.join("memory.db").exists() {
                return Ok(user_dir.clone());
            }
        }

        if let Some(global_dir) = global_dir.as_ref() {
            if Self::existing_database_is_writable(global_dir) {
                return Ok(global_dir.clone());
            }
        }

        user_dir
            .or(global_dir)
            .ok_or_else(|| MemoryError::Config("Could not determine data directory".into()))
    }

    fn existing_database_is_writable(dir: &Path) -> bool {
        let db_path = dir.join("memory.db");
        if !db_path.exists() {
            return false;
        }

        OpenOptions::new().write(true).open(&db_path).is_ok()
            && Self::directory_accepts_sidecar_writes(dir)
    }

    fn directory_accepts_sidecar_writes(dir: &Path) -> bool {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let probe_path = dir.join(format!(
            ".agent-memory-write-test-{}-{nanos}",
            std::process::id()
        ));

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe_path)
        {
            Ok(_) => {
                let _ = fs::remove_file(probe_path);
                true
            }
            Err(_) => false,
        }
    }

    pub fn ensure_dirs(&self) -> Result<(), MemoryError> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.model_cache_dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use super::GatewayConfig;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("agent-memory-config-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().expect("test path has parent")).expect("create parent");
        fs::write(path, b"").expect("touch db");
    }

    #[test]
    fn env_dir_overrides_existing_databases() {
        let root = temp_root();
        let env_dir = root.join("env");
        let user_dir = root.join("user");
        let global_dir = root.join("global");
        touch(&user_dir.join("memory.db"));
        touch(&global_dir.join("memory.db"));

        let resolved = Config::resolve_data_dir(
            Some(OsString::from(env_dir.as_os_str())),
            Some(user_dir),
            Some(global_dir),
        )
        .expect("resolve data dir");

        assert_eq!(resolved, env_dir);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn existing_user_database_wins_over_global_database() {
        let root = temp_root();
        let user_dir = root.join("user");
        let global_dir = root.join("global");
        touch(&user_dir.join("memory.db"));
        touch(&global_dir.join("memory.db"));

        let resolved = Config::resolve_data_dir(None, Some(user_dir.clone()), Some(global_dir))
            .expect("resolve data dir");

        assert_eq!(resolved, user_dir);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn existing_global_database_is_preserved_for_legacy_installs() {
        let root = temp_root();
        let user_dir = root.join("user");
        let global_dir = root.join("global");
        touch(&global_dir.join("memory.db"));

        let resolved = Config::resolve_data_dir(None, Some(user_dir), Some(global_dir.clone()))
            .expect("resolve data dir");

        assert_eq!(resolved, global_dir);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fresh_install_defaults_to_user_local_data_dir() {
        let root = temp_root();
        let user_dir = root.join("user");
        let global_dir = root.join("global");

        let resolved = Config::resolve_data_dir(None, Some(user_dir.clone()), Some(global_dir))
            .expect("resolve data dir");

        assert_eq!(resolved, user_dir);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gateway_config_prefers_memory_specific_env_values() {
        let cfg = GatewayConfig::from_values(
            Some(" https://memory-gateway.example ".to_string()),
            Some("https://gateway.example".to_string()),
            Some(" memory-key ".to_string()),
            Some("gateway-key".to_string()),
        );

        assert_eq!(
            cfg.base_url.as_deref(),
            Some("https://memory-gateway.example")
        );
        assert_eq!(cfg.api_key.as_deref(), Some("memory-key"));
    }

    #[test]
    fn gateway_config_falls_back_to_generic_gateway_env_values() {
        let cfg = GatewayConfig::from_values(
            Some(" ".to_string()),
            Some("https://gateway.example".to_string()),
            None,
            Some("gateway-key".to_string()),
        );

        assert_eq!(cfg.base_url.as_deref(), Some("https://gateway.example"));
        assert_eq!(cfg.api_key.as_deref(), Some("gateway-key"));
    }
}
