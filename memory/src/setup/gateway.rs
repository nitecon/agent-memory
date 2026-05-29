//! Gateway setup shared with `agent-tools`.

use crate::config::{user_gateway_conf_path, GatewayConfig};
use anyhow::{Context, Result};
use std::io::{self, BufRead, Write};

const DEFAULT_GATEWAY_URL: &str = "http://localhost:7913";
const DEFAULT_TIMEOUT_MS: &str = "5000";

pub fn run() -> Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut reader = stdin.lock();
    let existing = GatewayConfig::load();

    let default_url = existing.base_url.as_deref().unwrap_or(DEFAULT_GATEWAY_URL);
    write!(out, "Gateway URL [{default_url}]: ")?;
    out.flush()?;
    let mut url_input = String::new();
    reader
        .read_line(&mut url_input)
        .context("read gateway URL")?;
    let url = match url_input.trim() {
        "" => default_url,
        value => value,
    };

    let key_prompt = if existing.api_key.is_some() {
        "Gateway API key [keep existing]: "
    } else {
        "Gateway API key: "
    };
    let api_key_input = rpassword::prompt_password(key_prompt).context("failed to read API key")?;
    let api_key = match api_key_input.trim() {
        "" => existing
            .api_key
            .as_deref()
            .context("gateway API key is required")?,
        value => value,
    };
    validate_api_key(api_key)?;

    write!(out, "Request timeout in ms [{DEFAULT_TIMEOUT_MS}]: ")?;
    out.flush()?;
    let mut timeout_input = String::new();
    reader
        .read_line(&mut timeout_input)
        .context("read gateway timeout")?;
    let timeout = match timeout_input.trim() {
        "" => DEFAULT_TIMEOUT_MS.to_string(),
        value => value
            .parse::<u64>()
            .context("gateway timeout must be an integer number of milliseconds")?
            .to_string(),
    };

    let config_path = user_gateway_conf_path().context("home directory unavailable")?;
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }

    let existing_content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let content = upsert_key_value_lines(
        &existing_content,
        &[
            ("GATEWAY_URL", url),
            ("GATEWAY_API_KEY", api_key),
            ("GATEWAY_TIMEOUT_MS", &timeout),
        ],
    );
    std::fs::write(&config_path, content)
        .with_context(|| format!("write config to {}", config_path.display()))?;

    writeln!(out)?;
    writeln!(
        out,
        "Gateway config written to {} (shared with agent-tools)",
        config_path.display()
    )?;
    Ok(())
}

pub fn is_configured() -> bool {
    let cfg = GatewayConfig::load();
    cfg.base_url.is_some() && cfg.api_key.is_some()
}

pub fn configured_url() -> Option<String> {
    GatewayConfig::load().base_url
}

pub fn user_config_path_display() -> String {
    user_gateway_conf_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.agentic/agent-tools/gateway.conf".to_string())
}

fn validate_api_key(api_key: &str) -> Result<()> {
    if api_key.trim().is_empty() {
        anyhow::bail!("gateway API key is required");
    }
    if api_key.chars().any(char::is_control) {
        anyhow::bail!("gateway API key contains a control character");
    }
    Ok(())
}

fn upsert_key_value_lines(existing: &str, updates: &[(&str, &str)]) -> String {
    let mut seen = vec![false; updates.len()];
    let mut lines = Vec::new();

    for line in existing.lines() {
        let mut replaced = false;
        if let Some((key, _)) = line.trim().split_once('=') {
            let key = key.trim();
            if let Some((idx, (update_key, update_value))) = updates
                .iter()
                .enumerate()
                .find(|(_, (update_key, _))| key == *update_key)
            {
                lines.push(format!("{update_key}={update_value}"));
                seen[idx] = true;
                replaced = true;
            }
        }
        if !replaced {
            lines.push(line.to_string());
        }
    }

    for (idx, (key, value)) in updates.iter().enumerate() {
        if !seen[idx] {
            lines.push(format!("{key}={value}"));
        }
    }

    let mut content = lines.join("\n");
    content.push('\n');
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn upsert_preserves_unrelated_settings() {
        let content = upsert_key_value_lines(
            "DEFAULT_PROJECT_IDENT=agent-memory\nGATEWAY_URL=http://old\n",
            &[
                ("GATEWAY_URL", "https://gateway.example"),
                ("GATEWAY_API_KEY", "secret"),
            ],
        );

        assert!(content.contains("DEFAULT_PROJECT_IDENT=agent-memory\n"));
        assert!(content.contains("GATEWAY_URL=https://gateway.example\n"));
        assert!(content.contains("GATEWAY_API_KEY=secret\n"));
        assert!(!content.contains("http://old"));
    }

    #[test]
    fn read_key_value_file_parses_written_setup_format() {
        let path =
            std::env::temp_dir().join(format!("agent-memory-gateway-conf-{}.conf", Uuid::new_v4()));
        fs::write(
            &path,
            "GATEWAY_URL='https://gateway.example'\nGATEWAY_API_KEY=\"secret\"\n",
        )
        .expect("write temp config");

        let pairs = crate::config::read_key_value_file(&path).expect("read config");
        assert_eq!(
            pairs.get("GATEWAY_URL").map(String::as_str),
            Some("https://gateway.example")
        );
        assert_eq!(
            pairs.get("GATEWAY_API_KEY").map(String::as_str),
            Some("secret")
        );
        let _ = fs::remove_file(path);
    }
}
