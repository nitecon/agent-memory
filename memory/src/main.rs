mod cli;
mod concepts;
mod config;
mod db;
mod embedding;
mod error;
mod gateway_sync;
mod hook;
mod mcp;
mod project;
mod render;
mod search;
mod setup;
mod sync;
mod updater;

use clap::Parser;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;
use crate::config::Config;
use crate::db::open_database;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli {
        Cli::Serve => run_server(),
        // The hook path is special-cased like `Serve`: it must NEVER return a
        // non-zero exit, because on `UserPromptSubmit` a non-zero exit can block
        // the user's prompt in Claude. `run_hook_failsoft` swallows every error
        // and returns Ok(()) unconditionally.
        Cli::Hook { agent, limit } => {
            run_hook_failsoft(&agent, limit);
            Ok(())
        }
        other => run_cli(other),
    }
}

/// Fully fail-soft driver for `memory hook`. Wraps config load, DB open, and
/// retrieval so ANY error results in a clean exit 0 with no stdout. Unlike
/// [`run_cli`], this path deliberately does NOT run the auto-updater or install
/// noisy logging — the hook must be quiet and fast, and stdout must carry only
/// the envelope (or nothing). Diagnostics, if any, go to stderr.
fn run_hook_failsoft(agent: &str, limit: usize) {
    let Ok(config) = Config::load() else {
        return;
    };
    if config.ensure_dirs().is_err() {
        return;
    }
    let Ok(conn) = open_database(&config.db_path) else {
        return;
    };
    hook::run(agent, limit, &conn, &config);
}

fn run_cli(cli: Cli) -> anyhow::Result<()> {
    // CLI mode: stderr for logs, stdout for results
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let config = Config::load()?;
    config.ensure_dirs()?;

    // Auto-update check (rate-limited, non-blocking on failure)
    updater::auto_update(&config.data_dir);

    let conn = open_database(&config.db_path)?;

    cli::execute(cli, config, &conn)?;
    Ok(())
}

fn run_server() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        // MCP mode: stderr logging only, stdout is JSON-RPC transport
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();

        let config = Config::load()?;
        config.ensure_dirs()?;
        let conn = open_database(&config.db_path)?;

        let server = mcp::MemoryServer::new(config, conn);

        tracing::info!("Starting agent-memory MCP server");

        let service = server.serve(rmcp::transport::io::stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}
