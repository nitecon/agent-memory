use std::{
    collections::{BTreeSet, HashMap},
    env,
    io::Read,
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand, ValueEnum};
use rusqlite::Connection;

use crate::config::Config;
use crate::db::models::{
    embedding_to_blob, Memory, MemoryGatewaySync, MemoryGatewaySyncUpsert, WorkingContext,
    EMBEDDING_MODEL_NAME_DEFAULT,
};
use crate::db::queries::{self, ResolvedId};
use crate::embedding;
use crate::error::MemoryError;
use crate::project;
use crate::render;
use crate::search::{self, SearchOptions, SearchResult};
use crate::setup::{gateway, hooks, menu, rules, skill};
use crate::sync::{
    gateway_okf_envelope, memory_content_hash, GatewayMemory, GatewayMemoryProvenance,
    GatewayMemoryTombstone, GatewaySyncClientError, MemoryGatewayClient, PullMemoriesRequest,
    PullMemoriesResponse, PushMemoriesRequest, PushMemoriesResponse, PushMemoryAction,
};

/// Score multiplier applied to memories tagged with the current project.
/// Strong cross-project matches can still out-rank weak current-project hits;
/// the boost tilts ties toward local context without hard-filtering prior art.
pub const PROJECT_BOOST: f32 = 1.5;
/// Score multiplier applied to memories tagged with the global-scope sentinel
/// project. Intentionally smaller than `PROJECT_BOOST` so universal user
/// preferences surface in every repo while still losing ties to strong local
/// context.
pub const GLOBAL_BOOST: f32 = 1.25;
/// Reserved `project` ident for global-scoped memories. Users interact with
/// this value via `--scope global`, never by name. Guarded at the store/move
/// boundaries to prevent accidental collisions (e.g. a repo literally named
/// `__global__`).
pub const GLOBAL_PROJECT_IDENT: &str = "__global__";
pub const DEFAULT_PREVIEW_CHARS: usize = 160;
const DEFAULT_GATEWAY_PUSH_BATCH_SIZE: usize = 450;
const MAX_GATEWAY_PUSH_BATCH_SIZE: usize = 500;

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum OutputFormat {
    /// Compact: id, tags, project, quality flags, and a content preview.
    Brief,
    /// Full content plus all metadata.
    Full,
}

/// Logical scope for a new memory. Projects receive the 1.5× current-project
/// boost during retrieval from the matching cwd; `Global` memories are
/// stored under the reserved sentinel project and receive a 1.25× boost from
/// every cwd so universal preferences surface across all repos.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum MemoryScope {
    /// Scoped to the current project (cwd-derived ident, overridable via
    /// `--project`). This is the default.
    Project,
    /// Universal — applies across every repo. Stored under the reserved
    /// sentinel project ident and boosted in every `context`/`search` call.
    Global,
}

#[derive(Parser)]
#[command(name = "memory", about = "Persistent hybrid-search memory system for AI coding agents", version = env!("AGENT_MEMORY_VERSION"))]
pub enum Cli {
    /// Read, validate, write, traverse, import, and export OKF-native memories.
    Okf {
        #[command(subcommand)]
        command: OkfCommands,
    },
    /// Save a memory with auto-embedding and BM25 indexing.
    ///
    /// If --project is omitted, the current project is auto-detected from the
    /// working directory's git remote or canonical path. Use `--scope global`
    /// to store a universal preference that applies across every repo.
    Store {
        /// Memory content text.
        content: String,
        /// Optional OKF Markdown document; its body and metadata replace the
        /// plain positional content while preserving the legacy invocation.
        #[arg(long)]
        okf_file: Option<PathBuf>,
        /// Comma-separated tags.
        #[arg(short, long)]
        tags: Option<String>,
        /// Project identifier (defaults to cwd-derived ident).
        #[arg(short, long)]
        project: Option<String>,
        /// Agent identifier.
        #[arg(short, long)]
        agent: Option<String>,
        /// Source file path.
        #[arg(short = 'f', long)]
        source_file: Option<String>,
        /// Memory type: user, feedback, project, reference.
        #[arg(short = 'm', long, default_value = "user")]
        memory_type: String,
        /// Store without any project tag (skips cwd auto-detection).
        #[arg(long)]
        no_project: bool,
        /// Scope for the memory. `project` (default) stores under the cwd
        /// ident; `global` stores under the reserved sentinel so the memory
        /// is boosted across every repo. Left as `Option` (no default) so
        /// the dispatch layer can tell whether the user chose deliberately
        /// and only emit reflection hints when they didn't.
        #[arg(long, value_enum)]
        scope: Option<MemoryScope>,
    },
    /// Hybrid BM25 + vector search with current-project boost.
    ///
    /// By default, the current project (from cwd) is boosted but cross-project
    /// results can still surface. Use --only to hard-filter, or
    /// --no-project-boost for flat ranking.
    Search {
        /// Search query.
        query: String,
        /// Number of results.
        #[arg(short = 'k', long, default_value = "10")]
        limit: usize,
        /// Project to boost (defaults to cwd-derived ident).
        #[arg(short, long)]
        project: Option<String>,
        /// Hard filter: only return memories from this project.
        #[arg(long)]
        only: Option<String>,
        /// Disable the current-project boost entirely.
        #[arg(long)]
        no_project_boost: bool,
        /// Exact OKF concept type filter (for example `Agent Memory/project`).
        #[arg(long)]
        concept_type: Option<String>,
        /// Exact tag filter.
        #[arg(long)]
        tag: Option<String>,
        /// Text-seeded graph expansion depth (0 disables; capped at 2 for retrieval).
        #[arg(long, default_value_t = 1)]
        graph_depth: usize,
        /// Output format (default: brief).
        #[arg(long, value_enum, default_value_t = OutputFormat::Brief)]
        format: OutputFormat,
        /// Preview length for brief output.
        #[arg(long, default_value_t = DEFAULT_PREVIEW_CHARS)]
        preview_chars: usize,
    },
    /// Filter memories by project/agent/tags.
    Recall {
        /// Filter by project.
        #[arg(short, long)]
        project: Option<String>,
        /// Filter by agent.
        #[arg(short, long)]
        agent: Option<String>,
        /// Comma-separated tags to filter by.
        #[arg(short, long)]
        tags: Option<String>,
        /// Filter by memory type.
        #[arg(short = 'm', long)]
        memory_type: Option<String>,
        /// Number of results.
        #[arg(short = 'k', long, default_value = "10")]
        limit: usize,
        /// Output format (default: brief).
        #[arg(long, value_enum, default_value_t = OutputFormat::Brief)]
        format: OutputFormat,
        /// Preview length for brief output.
        #[arg(long, default_value_t = DEFAULT_PREVIEW_CHARS)]
        preview_chars: usize,
    },
    /// Remove memories by ID or search.
    Forget {
        /// Memory ID to remove.
        #[arg(short, long)]
        id: Option<String>,
        /// Search query to find and remove memories.
        #[arg(short, long)]
        query: Option<String>,
    },
    /// Backfill structurally derived OKF metadata (title, description, sources).
    ///
    /// Recall and `get` already enrich the memories they touch, so this only
    /// exists to sweep a store in one pass rather than waiting for each row to
    /// be used. Derivation is structural — no model is loaded and no network
    /// call is made — and it never replaces an authored value.
    Enrich {
        /// Limit to one project ident (defaults to every project).
        #[arg(short, long)]
        project: Option<String>,
        /// Report what would be filled without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Decay stale/low-access memories.
    Prune {
        /// Maximum age in days before pruning.
        #[arg(short, long, default_value = "90")]
        max_age_days: u64,
        /// Minimum access count to keep.
        #[arg(short = 'c', long, default_value = "0")]
        min_access_count: i64,
        /// Show what would be pruned without deleting.
        #[arg(long)]
        dry_run: bool,
    },
    /// Return top-K relevant memories for a task, with current-project boost.
    Context {
        /// Task description.
        description: String,
        /// Number of results.
        #[arg(short = 'k', long, default_value = "5")]
        limit: usize,
        /// Project to boost (defaults to cwd-derived ident).
        #[arg(short, long)]
        project: Option<String>,
        /// Hard filter: only return memories from this project.
        #[arg(long)]
        only: Option<String>,
        /// Disable the current-project boost entirely.
        #[arg(long)]
        no_project_boost: bool,
        /// Omit the project's WorkingContext from the output. Used by the
        /// per-turn memory-injection hook, where WorkingContext is already
        /// injected once by the SessionStart hook (and refreshed across
        /// compaction) — re-emitting it every turn is pure duplication.
        #[arg(long)]
        no_working_context: bool,
        /// Output format (default: brief).
        #[arg(long, value_enum, default_value_t = OutputFormat::Brief)]
        format: OutputFormat,
        /// Preview length for brief output.
        #[arg(long, default_value_t = DEFAULT_PREVIEW_CHARS)]
        preview_chars: usize,
    },
    /// Fetch full content for one or more memory IDs.
    ///
    /// Pair with `memory search --brief` for a cheap two-stage flow: scan
    /// lightweight hits, then pull the full content of the handful you want.
    Get {
        /// Memory IDs to fetch.
        #[arg(required = true)]
        ids: Vec<String>,
        /// Output format (default: full).
        #[arg(long, value_enum, default_value_t = OutputFormat::Full)]
        format: OutputFormat,
        /// Preview length when --format brief is used.
        #[arg(long, default_value_t = DEFAULT_PREVIEW_CHARS)]
        preview_chars: usize,
    },
    /// List all stored memories.
    List {
        /// Number of results.
        #[arg(short = 'k', long, default_value = "50")]
        limit: usize,
        /// Filter by project.
        #[arg(short, long)]
        project: Option<String>,
        /// Filter by memory type.
        #[arg(short = 'm', long)]
        memory_type: Option<String>,
        /// Output format (default: brief).
        #[arg(long, value_enum, default_value_t = OutputFormat::Brief)]
        format: OutputFormat,
        /// Preview length for brief output.
        #[arg(long, default_value_t = DEFAULT_PREVIEW_CHARS)]
        preview_chars: usize,
    },
    /// Reassign the `project` ident on one or more memories.
    ///
    /// Common use: migrate memories that were tagged under a legacy project
    /// name (e.g. `trading-platform-sre`) to the canonical git-remote ident
    /// (e.g. `github.com/nitecon/SRE.git`) the cwd-resolver returns now.
    /// Project-wide moves also transfer the source WorkingContext to the
    /// target project; moving to `--to ""` clears it.
    ///
    /// Selectors:
    ///   --id <ID>        move a single memory
    ///   --from <PROJ>    move every memory currently tagged with <PROJ>
    ///                    (pass `--from ""` to target memories with no project)
    ///
    /// Target:
    ///   --to <PROJ>      new project ident (pass `--to ""` to clear)
    Move {
        /// Move a single memory by ID.
        #[arg(long, conflicts_with = "from")]
        id: Option<String>,
        /// Move all memories whose current project equals this value.
        /// Use an empty string ("") to target memories with no project.
        #[arg(long)]
        from: Option<String>,
        /// New project ident. Use an empty string ("") to clear the project tag.
        #[arg(long)]
        to: String,
        /// Show the memories that would be moved without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Duplicate one or more memories under a new project ident.
    ///
    /// WorkingContext is not copied. Content, tags, agent, source_file,
    /// memory_type, and the cached embedding are all preserved on the copy.
    /// A new UUID is minted and timestamps reset; the source row is left
    /// untouched.
    ///
    /// Selectors mirror `memory move`:
    ///   --id <ID>        copy a single memory
    ///   --from <PROJ>    copy every memory currently tagged with <PROJ>
    Copy {
        /// Copy a single memory by ID.
        #[arg(long, conflicts_with = "from")]
        id: Option<String>,
        /// Copy all memories whose current project equals this value.
        /// Use an empty string ("") to target memories with no project.
        #[arg(long)]
        from: Option<String>,
        /// New project ident for the copies. Use an empty string ("") to
        /// create copies with no project tag.
        #[arg(long)]
        to: String,
        /// Show the memories that would be copied without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// List distinct durable-memory project idents with memory counts.
    ///
    /// Useful for spotting alias mismatches (e.g. `trading-platform-sre` vs
    /// `github.com/nitecon/SRE.git`) before running `memory move --from … --to …`.
    Projects,
    /// Manage the current project's live WorkingContext handoff.
    Working {
        #[command(subcommand)]
        command: WorkingCommands,
    },
    /// Push project-scoped durable memories to the agent gateway.
    Push {
        /// Push every local durable-memory project, including global scope.
        #[arg(long)]
        all: bool,
        /// Maximum memory records to send in one gateway request.
        #[arg(long, default_value_t = DEFAULT_GATEWAY_PUSH_BATCH_SIZE)]
        batch_size: usize,
        #[command(subcommand)]
        command: Option<GatewayTransferCommand>,
    },
    /// Pull project-scoped durable memories from the agent gateway.
    Pull {
        /// Discover and pull every gateway memory project, including global scope.
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        command: Option<GatewayTransferCommand>,
    },
    /// Start MCP stdio server.
    Serve,
    /// Runtime hook entrypoint invoked by an agent CLI's per-turn hook (installed
    /// by `memory setup hooks`). Reads the hook JSON payload on stdin, runs the
    /// per-turn memory retrieval, and emits a `hookSpecificOutput` envelope on
    /// stdout for the agent to inject as additionalContext. Fail-soft: always
    /// exits 0.
    Hook {
        /// Agent name (claude, codex, gemini). Selects the hook event name.
        #[arg(long, default_value = "claude")]
        agent: String,
        /// Number of memories to retrieve.
        #[arg(short = 'k', long, default_value = "5")]
        limit: usize,
    },
    /// Dual-mode command:
    ///
    ///   - `memory update`                  → check for and install the
    ///     latest `memory` release (self-updater; original behavior).
    ///   - `memory update <id> --content X` → atomically re-author the
    ///     memory at `<id>`: replace content, archive the prior body to
    ///     `content_raw`, bump `updated_at`, clear `superseded_by`, and
    ///     re-embed. Supports short ID prefixes (≥4 chars).
    ///
    /// The two modes are distinguished purely by presence of the positional
    /// `<id>` argument. This lets the agentic dream pass use the CLI contract
    /// documented in `docs/plan` while preserving the pre-2.3 self-updater
    /// invocation verbatim. Primary writer of the content-update form is
    /// `memory-dream` running under a tool-enabled LLM backend.
    Update {
        /// Memory ID (full UUID or ≥4-char prefix). When omitted, the
        /// command runs the self-updater. When provided, `--content` is
        /// required.
        id: Option<String>,
        /// New content body. Required when `id` is supplied; ignored by the
        /// self-updater path.
        #[arg(long)]
        content: Option<String>,
        /// Re-author from a complete OKF Markdown document.
        #[arg(long, conflicts_with = "content")]
        okf_file: Option<PathBuf>,
        /// Optional comma-separated tag replacement. Omit to preserve
        /// existing tags. Ignored by the self-updater path.
        #[arg(long)]
        tags: Option<String>,
        /// Optional memory type replacement (user | feedback | project | reference).
        /// Omit to preserve the existing type. Ignored by the self-updater path.
        #[arg(short = 'm', long)]
        memory_type: Option<String>,
    },
    /// Setup and configuration commands (run with no subcommand for an
    /// interactive checklist of available components).
    Setup {
        #[command(subcommand)]
        command: Option<SetupCommands>,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum GraphDirection {
    In,
    Out,
    Both,
}

#[derive(Subcommand)]
pub enum OkfCommands {
    /// Validate an OKF Markdown document.
    Validate { file: PathBuf },
    /// Render one canonical memory as OKF Markdown.
    Get { target: String },
    /// Create or update a canonical OKF memory.
    Put {
        target: String,
        #[arg(long, default_value = "-")]
        file: PathBuf,
        #[arg(long)]
        expect_revision: Option<i64>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Read a virtual bundle document.
    Read { bundle: String, path: String },
    /// List a virtual bundle directory.
    List {
        bundle: String,
        #[arg(default_value = "/")]
        path: String,
    },
    /// Render a root, type, or tag index.
    Index {
        bundle: String,
        #[arg(long = "type", conflicts_with = "tag")]
        concept_type: Option<String>,
        #[arg(long)]
        tag: Option<String>,
    },
    /// Render paginated immutable history for a bundle.
    Log {
        bundle: String,
        #[arg(long)]
        cursor: Option<usize>,
        #[arg(short = 'k', long, default_value_t = 50)]
        limit: usize,
    },
    /// List immutable revisions for one memory.
    History {
        id: String,
        #[arg(short = 'k', long, default_value_t = 20)]
        limit: usize,
    },
    /// Compare two immutable revisions.
    Diff { id: String, rev_a: i64, rev_b: i64 },
    /// Traverse typed memory relationships.
    Graph {
        target: String,
        #[arg(long)]
        relation: Option<String>,
        #[arg(long, value_enum, default_value_t = GraphDirection::Out)]
        direction: GraphDirection,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(short = 'k', long, default_value_t = 100)]
        limit: usize,
    },
    /// Export an explicit physical projection of a virtual bundle.
    Export {
        bundle: String,
        target: PathBuf,
        #[arg(long = "id")]
        ids: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Import a physical OKF bundle through the canonical handlers.
    Import {
        source: PathBuf,
        #[arg(long, conflicts_with = "scope")]
        project: Option<String>,
        #[arg(long, value_enum)]
        scope: Option<MemoryScope>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub enum WorkingCommands {
    /// Print the current project's WorkingContext, if any.
    Get {
        /// Project identifier (defaults to cwd-derived ident).
        #[arg(short, long)]
        project: Option<String>,
    },
    /// Replace the current project's WorkingContext.
    Set {
        /// WorkingContext body. Use "-" to read from stdin.
        content: Option<String>,
        /// Read WorkingContext body from a file.
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        /// Project identifier (defaults to cwd-derived ident).
        #[arg(short, long)]
        project: Option<String>,
    },
    /// Clear the current project's WorkingContext.
    Clear {
        /// Project identifier (defaults to cwd-derived ident).
        #[arg(short, long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum GatewayTransferCommand {
    /// Show pending gateway exchange actions without local mutation.
    Status,
}

#[derive(Subcommand)]
pub enum SetupCommands {
    /// Configure the shared agent-gateway connection used by memory push/pull.
    ///
    /// Writes `GATEWAY_URL`, `GATEWAY_API_KEY`, and `GATEWAY_TIMEOUT_MS` to
    /// `~/.agentic/agent-tools/gateway.conf`, the same config file consumed by
    /// `agent-tools setup gateway`.
    Gateway,

    /// Inject the memory usage protocols into known agent rule files.
    ///
    /// Detects `~/.claude/CLAUDE.md`, `~/.gemini/GEMINI.md`,
    /// `~/.codex/AGENTS.md`, and `~/.config/codex/AGENTS.md`, then writes a
    /// `<memory-rules>…</memory-rules>` block so the agent knows how to call
    /// the `memory` CLI. Idempotent — re-runs replace the existing block in
    /// place. A `.bak` sibling is written before each modification.
    ///
    /// If an `<agent-tools-rules>` block is already present (written by the
    /// sibling `agent-tools setup rules` command), the memory block is
    /// inserted directly after it; otherwise it is prepended.
    Rules {
        /// Update a specific file instead of running detection.
        #[arg(long)]
        target: Option<PathBuf>,
        /// Update every detected file without prompting.
        #[arg(long)]
        all: bool,
        /// Show the resulting file content without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Print the rules block to stdout and exit (no file IO).
        #[arg(long)]
        print: bool,
        /// Strip the `<memory-rules>` block from detected rule files and
        /// remove the paired `autoMemoryEnabled` key from any Claude
        /// `settings.json`. Inverse of the default install.
        #[arg(long)]
        remove: bool,
    },

    /// Install an Agent Skill so the `memory` CLI is auto-advertised to
    /// sessions via the always-loaded skill description (~100 tokens). The
    /// full body only loads on demand.
    ///
    /// Writes `SKILL.md` to every known skill-root unconditionally:
    ///   - `~/.claude/skills/agent-memory/SKILL.md` (Claude Code, tool-native)
    ///   - `~/.agents/skills/agent-memory/SKILL.md` (cross-agent alias — read
    ///     by Gemini CLI, Codex, and any future frontend honoring the shared
    ///     `.agents/skills/` convention)
    ///
    /// Every frontend reads the same YAML frontmatter + Markdown body, so
    /// identical byte contents are written to each target. No auto-detection
    /// of whether the agent is installed — running `memory setup skill` is
    /// the opt-in signal. Legacy install paths (v1.4.0/v1.4.1's
    /// `~/.gemini/skills/agent-memory/SKILL.md`) are scrubbed on every
    /// install and every `--remove`.
    Skill {
        /// Show the resulting file content without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Print the SKILL.md to stdout and exit (no file IO).
        #[arg(long)]
        print: bool,
        /// Delete the installed `SKILL.md` from every known target. Missing
        /// files are a silent no-op per target. Inverse of the default
        /// install, matching `setup rules --remove`.
        #[arg(long)]
        remove: bool,
    },

    /// Wire automatic per-turn RAG memory injection into each agent CLI's
    /// hook system (POC — opt-in, NOT part of `setup all`).
    ///
    /// Scriptless: registers the `memory hook --agent <agent>` subcommand
    /// directly in each detected agent's per-turn hook (no shared `.sh`
    /// script — a binary subcommand is cross-platform, with no bash/jq
    /// dependency and no `.sh` file-association surprises on Windows):
    ///   - Claude Code → `UserPromptSubmit` in `~/.claude/settings.json`
    ///     → `memory hook --agent claude`
    ///   - Gemini CLI  → `BeforeAgent` in `~/.gemini/settings.json`
    ///     → `memory hook --agent gemini`
    ///   - Codex CLI   → `UserPromptSubmit` in Codex `config.toml`
    ///     → `memory hook --agent codex`
    ///
    /// On each turn the subcommand reads the prompt from the hook payload,
    /// runs `memory context`, and injects the result via `additionalContext`
    /// — so the agent no longer has to call `memory context` itself.
    /// Idempotent: re-runs replace our entry in place, and upgrades strip any
    /// stale `memory-inject.sh` script entry left by older installs. This
    /// complements (and lets you trim) the manual-recall part of the
    /// `<memory-rules>` block written by `setup rules`.
    Hooks {
        /// Show the intended actions without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Print the per-agent installed commands to stdout and exit (no file IO).
        #[arg(long)]
        print: bool,
        /// Strip our hook entries from each detected agent and delete the
        /// shared bridge script. Inverse of the default install.
        #[arg(long)]
        remove: bool,
    },

    /// Run gateway → rules → skill in sequence.
    All {
        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

pub fn execute(cmd: Cli, config: Config, conn: &Connection) -> Result<(), MemoryError> {
    let cwd_project = project::project_ident_from_cwd().ok();

    match cmd {
        Cli::Okf { command } => {
            execute_okf(command, conn, cwd_project.as_deref())?;
        }
        Cli::Store {
            content,
            okf_file,
            tags,
            project,
            agent,
            source_file,
            memory_type,
            no_project,
            scope,
        } => {
            let tag_list = tags.map(|t| t.split(',').map(|s| s.trim().to_string()).collect());

            // Reject accidental sentinel collision on --project before we
            // resolve the scope. Users who want global scope must say so
            // explicitly via `--scope global` so the intent is recorded in
            // shell history rather than hiding behind a suspicious string.
            if matches!(scope, Some(MemoryScope::Project) | None)
                && project.as_deref() == Some(GLOBAL_PROJECT_IDENT)
            {
                return Err(MemoryError::Config(format!(
                    "`{GLOBAL_PROJECT_IDENT}` is reserved for global-scoped memories. \
                     Use `--scope global` instead."
                )));
            }

            // Normalize scope: None means "user didn't pass --scope" — treat
            // it as project-scoped for storage, but remember the fact so the
            // reflection hint below can nudge them.
            let scope_explicit = scope.is_some();
            let resolved_scope = scope.unwrap_or(MemoryScope::Project);

            let resolved_project = match resolved_scope {
                MemoryScope::Global => {
                    // --scope global forces the sentinel regardless of any
                    // --project override or cwd auto-detect. --no-project is
                    // incompatible with global scope (a global memory *must*
                    // live under the sentinel).
                    if no_project {
                        return Err(MemoryError::Config(
                            "--no-project is incompatible with --scope global".to_string(),
                        ));
                    }
                    Some(GLOBAL_PROJECT_IDENT.to_string())
                }
                MemoryScope::Project => {
                    if no_project {
                        None
                    } else {
                        project.or(cwd_project.clone())
                    }
                }
            };

            let memory = if let Some(okf_file) = okf_file {
                let text = read_text_path(&okf_file)?;
                let parsed = agent_memory::okf::parse_document(&text)
                    .map_err(|error| MemoryError::Config(error.to_string()))?;
                let emb = embedding::embed_text(&parsed.concept.body, &config.model_cache_dir)?;
                let scope = match resolved_scope {
                    MemoryScope::Global => agent_memory::okf::BundleScope::Global,
                    MemoryScope::Project => resolved_project
                        .as_ref()
                        .map(|project| agent_memory::okf::BundleScope::Project(project.clone()))
                        .unwrap_or(agent_memory::okf::BundleScope::Unscoped),
                };
                let put = agent_memory::okf::OkfDocumentHandler::new(conn, scope)
                    .put(None, &parsed, None, false)
                    .map_err(okf_cli_error)?;
                let blob = embedding_to_blob(&emb);
                conn.execute(
                    "UPDATE memories SET embedding = ?1, embedding_model = ?2 WHERE id = ?3",
                    rusqlite::params![blob, EMBEDDING_MODEL_NAME_DEFAULT, put.id],
                )?;
                queries::get_memory_by_id(conn, &put.id)?
            } else {
                let mut memory = Memory::new(
                    content.clone(),
                    tag_list,
                    resolved_project,
                    agent,
                    source_file,
                    Some(memory_type.clone()),
                );
                let emb = embedding::embed_text(&content, &config.model_cache_dir)?;
                memory.embedding = Some(emb);
                queries::insert_memory(conn, &memory)?;
                memory
            };

            let mut attrs: Vec<(&str, String)> = vec![
                ("id", render::short_id(&memory.id).to_string()),
                ("scope", scope_label(resolved_scope).to_string()),
            ];
            if let Some(p) = memory.project.as_deref() {
                attrs.push(("project", p.to_string()));
            }
            println!("{}", render::render_action_result("stored", &attrs));
            println!("{}", render::render_hint(store_quality_hint()));
            if let Some(canonical) = canonical_state_hint(&content) {
                println!("{}", render::render_hint(canonical));
            }
            // Reflection hint: nudge the agent to reconsider scope only when
            // the memory type is most likely to be cross-cutting (user or
            // feedback) AND the user didn't pick a scope deliberately. No
            // noise on `project`/`reference` stores or when --scope was
            // passed explicitly — silence is the default.
            if !scope_explicit
                && resolved_scope == MemoryScope::Project
                && matches!(memory_type.as_str(), "user" | "feedback")
            {
                println!("{}", render::render_hint(&store_scope_hint()));
            }
            maybe_auto_sync_after_store(conn, &config, &memory);
        }
        Cli::Search {
            query,
            limit,
            project,
            only,
            no_project_boost,
            concept_type,
            tag,
            graph_depth,
            format: _,
            preview_chars: _,
        } => {
            let boosts =
                resolve_boosts(project.as_deref(), cwd_project.as_deref(), no_project_boost);
            let opts = SearchOptions {
                limit,
                current_project: boosts.current_project,
                boost_factor: boosts.project_boost,
                only_project: only.as_deref(),
                global_project: boosts.global_project,
                global_boost_factor: boosts.global_boost,
                concept_type: concept_type.as_deref(),
                tag: tag.as_deref(),
                graph_depth,
            };
            let results = search::hybrid_search(conn, &query, opts, &config.model_cache_dir)?;
            print_ranked(&results, &boosts, &query, None, false);
        }
        Cli::Context {
            description,
            limit,
            project,
            only,
            no_project_boost,
            no_working_context,
            format: _,
            preview_chars: _,
        } => {
            // Shared retrieval+render path (also used by `memory hook`): keeps
            // the `memory context` output byte-for-byte identical while letting
            // the hook reuse the exact same logic. `println!` re-adds the
            // trailing newline `print_ranked` would have emitted.
            let block = retrieve_context_block(
                conn,
                &config,
                &description,
                limit,
                no_working_context,
                project.as_deref(),
                cwd_project.as_deref(),
                no_project_boost,
                only.as_deref(),
            )?;
            println!("{block}");
        }
        Cli::Recall {
            project,
            agent,
            tags,
            memory_type,
            limit,
            format: _,
            preview_chars: _,
        } => {
            let tag_list: Option<Vec<String>> =
                tags.map(|t| t.split(',').map(|s| s.trim().to_string()).collect());

            let memories = queries::list_memories(
                conn,
                project.as_deref(),
                agent.as_deref(),
                tag_list.as_deref(),
                memory_type.as_deref(),
                limit,
            )?;

            println!(
                "{}",
                render::render_memory_list(&memories, cwd_project.as_deref())
            );
            println!("{}", render::render_usage_legend());
        }
        Cli::Forget { id, query } => {
            if let Some(id) = id {
                match queries::resolve_id_prefix(conn, &id)? {
                    ResolvedId::Exact(full_id) => {
                        let memory = queries::get_memory_by_id(conn, &full_id)?;
                        crate::gateway_sync::tombstone_memory_before_local_removal(
                            conn,
                            &config.gateway,
                            &memory,
                            "memory forgotten locally",
                        )?;
                        let deleted = queries::delete_memory(conn, &full_id)?;
                        let status = if deleted { "forgot" } else { "not_found" };
                        let short = render::short_id(&full_id).to_string();
                        println!("{}", render::render_action_result(status, &[("id", short)]));
                    }
                    ResolvedId::Ambiguous(cands) => {
                        println!("{}", render::render_ambiguous(&id, &cands));
                    }
                    ResolvedId::NotFound => {
                        println!(
                            "{}",
                            render::render_action_result("not_found", &[("id", id)])
                        );
                    }
                }
            } else if let Some(query) = query {
                let opts = SearchOptions::new(5);
                let results = search::hybrid_search(conn, &query, opts, &config.model_cache_dir)?;
                if results.is_empty() {
                    println!("{}", render::render_action_result("no_matches", &[]));
                } else {
                    let mut deleted = 0usize;
                    for r in &results {
                        crate::gateway_sync::tombstone_memory_before_local_removal(
                            conn,
                            &config.gateway,
                            &r.memory,
                            "memory forgotten locally",
                        )?;
                        if queries::delete_memory(conn, &r.memory.id)? {
                            deleted += 1;
                        }
                    }
                    println!(
                        "{}",
                        render::render_action_result("forgot", &[("count", deleted.to_string())])
                    );
                }
            } else {
                eprintln!("Either --id or --query must be provided");
            }
        }
        Cli::Enrich { project, dry_run } => {
            let ids = queries::list_enrichable_ids(conn, project.as_deref())?;
            let mut filled_counts: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            let mut touched = 0usize;
            for id in &ids {
                let filled = if dry_run {
                    // Derive without writing, then report only what would
                    // actually land — absent fields, not every derivable one.
                    crate::concepts::enrich::preview(conn, id)?
                } else {
                    crate::concepts::enrich::enrich(conn, id)?
                };
                if filled.is_empty() {
                    continue;
                }
                touched += 1;
                for field in filled {
                    *filled_counts.entry(field).or_default() += 1;
                }
            }
            let status = if dry_run { "dry_run" } else { "enriched" };
            let mut fields = vec![
                ("scanned", ids.len().to_string()),
                ("memories", touched.to_string()),
            ];
            for (field, count) in &filled_counts {
                fields.push((field.as_str(), count.to_string()));
            }
            println!("{}", render::render_action_result(status, &fields));
        }
        Cli::Prune {
            max_age_days,
            min_access_count,
            dry_run,
        } => {
            let pruned = queries::prune_memories(conn, max_age_days, min_access_count, dry_run)?;
            let status = if dry_run { "dry_run" } else { "pruned" };
            println!(
                "{}",
                render::render_action_result(status, &[("count", pruned.len().to_string())])
            );
        }
        Cli::Get {
            ids,
            format: _,
            preview_chars: _,
        } => {
            // Each arg is resolved independently: a short prefix that maps to
            // one memory produces a <memory>, an ambiguous prefix produces an
            // <ambiguous> block, and a miss produces a `<result status="not_found"
            // id="..."/>` — all routed through the renderer so the surface stays
            // consistent.
            let mut fetched: Vec<Memory> = Vec::with_capacity(ids.len());
            let mut out_lines: Vec<String> = Vec::with_capacity(ids.len());
            for id in &ids {
                match queries::resolve_id_prefix(conn, id)? {
                    ResolvedId::Exact(full_id) => match queries::get_memory_by_id(conn, &full_id) {
                        Ok(m) => {
                            // Same enrich-on-use contract as recall: fetching a
                            // memory is a use, and derived descriptors cost no
                            // inference.
                            crate::concepts::enrich::enrich_quietly(conn, &full_id);
                            out_lines.push(render::render_memory(&m));
                            fetched.push(m);
                        }
                        Err(MemoryError::NotFound(_)) => {
                            out_lines.push(render::render_action_result(
                                "not_found",
                                &[("id", id.clone())],
                            ));
                        }
                        Err(e) => return Err(e),
                    },
                    ResolvedId::Ambiguous(cands) => {
                        out_lines.push(render::render_ambiguous(id, &cands));
                    }
                    ResolvedId::NotFound => {
                        out_lines.push(render::render_action_result(
                            "not_found",
                            &[("id", id.clone())],
                        ));
                    }
                }
            }
            let hit_ids: Vec<String> = fetched.iter().map(|m| m.id.clone()).collect();
            queries::increment_access(conn, &hit_ids)?;
            for line in &out_lines {
                println!("{line}");
            }
        }
        Cli::List {
            limit,
            project,
            memory_type,
            format: _,
            preview_chars: _,
        } => {
            let memories = queries::list_memories(
                conn,
                project.as_deref(),
                None,
                None,
                memory_type.as_deref(),
                limit,
            )?;
            println!(
                "{}",
                render::render_memory_list(&memories, cwd_project.as_deref())
            );
            println!("{}", render::render_usage_legend());
        }
        Cli::Move {
            id,
            from,
            to,
            dry_run,
        } => {
            // Guard: writing the reserved sentinel via `--to` would bypass
            // the `--scope global` contract. If a user legitimately wants to
            // promote an existing memory to global scope, the follow-up
            // `memory promote`/`memory demote` helpers (deferred) or a
            // future `--allow-sentinel` escape hatch are the right surface.
            if to == GLOBAL_PROJECT_IDENT {
                return Err(MemoryError::Config(format!(
                    "`{GLOBAL_PROJECT_IDENT}` is reserved for global-scoped memories. \
                     Use `memory store --scope global <content>` for new globals, or \
                     re-run with a different `--to` value."
                )));
            }
            let new_project = empty_to_none(&to);
            let resolved_id = resolve_id_arg(conn, id.as_deref())?;
            run_move(
                conn,
                resolved_id.as_deref(),
                from.as_deref(),
                new_project,
                dry_run,
            )?;
        }
        Cli::Copy {
            id,
            from,
            to,
            dry_run,
        } => {
            let new_project = empty_to_none(&to);
            let resolved_id = resolve_id_arg(conn, id.as_deref())?;
            run_copy(
                conn,
                resolved_id.as_deref(),
                from.as_deref(),
                new_project,
                dry_run,
            )?;
        }
        Cli::Projects => {
            let rows = queries::list_projects(conn)?;
            println!("{}", render::render_projects(&rows, cwd_project.as_deref()));
        }
        Cli::Working { command } => match command {
            WorkingCommands::Get { project } => {
                let project = resolve_working_project(project, cwd_project.as_deref())?;
                let ctx = queries::get_working_context(conn, &project)?;
                println!("{}", render::render_working_context(ctx.as_ref(), &project));
            }
            WorkingCommands::Set {
                project,
                content,
                file,
            } => {
                let project = resolve_working_project(project, cwd_project.as_deref())?;
                let content = read_working_content(content, file)?;
                let ctx = queries::set_working_context(conn, &project, &content)?;
                println!(
                    "{}",
                    render::render_action_result(
                        "working_context_set",
                        &[
                            ("project", ctx.project),
                            ("version", ctx.version.to_string()),
                            ("updated_at", ctx.updated_at),
                        ],
                    )
                );
            }
            WorkingCommands::Clear { project } => {
                let project = resolve_working_project(project, cwd_project.as_deref())?;
                let deleted = queries::clear_working_context(conn, &project)?;
                println!(
                    "{}",
                    render::render_action_result(
                        "working_context_cleared",
                        &[("project", project), ("deleted", deleted.to_string())],
                    )
                );
            }
        },
        Cli::Push {
            all,
            batch_size,
            command,
        } => {
            run_push(
                conn,
                &config,
                cwd_project.as_deref(),
                matches!(command, Some(GatewayTransferCommand::Status)),
                batch_size,
                all,
            )?;
        }
        Cli::Pull { all, command } => {
            run_pull(
                conn,
                &config,
                cwd_project.as_deref(),
                matches!(command, Some(GatewayTransferCommand::Status)),
                all,
            )?;
        }
        Cli::Serve => {
            unreachable!("Serve is handled in main.rs");
        }
        Cli::Hook { .. } => {
            unreachable!("Hook is handled in main.rs (fail-soft path)");
        }
        Cli::Update {
            id,
            content,
            okf_file,
            tags,
            memory_type,
        } => match id {
            None => {
                if content.is_some()
                    || okf_file.is_some()
                    || tags.is_some()
                    || memory_type.is_some()
                {
                    return Err(MemoryError::Config(
                        "`memory update --content ...` requires a positional <id>. \
                             Run `memory update <id> --content \"...\"` to re-author, or \
                             `memory update` alone to run the self-updater."
                            .to_string(),
                    ));
                }
                crate::updater::manual_update()?;
            }
            Some(raw_id) => {
                if let Some(okf_file) = okf_file {
                    run_update_okf(conn, &config, &raw_id, &okf_file)?;
                } else {
                    let new_content = content.ok_or_else(|| {
                        MemoryError::Config(
                            "`memory update <id>` requires --content \"...\" or --okf-file FILE"
                                .to_string(),
                        )
                    })?;
                    run_update_content(
                        conn,
                        &config,
                        &raw_id,
                        &new_content,
                        tags.as_deref(),
                        memory_type.as_deref(),
                    )?;
                }
            }
        },
        Cli::Setup { command } => {
            execute_setup(command).map_err(|e| MemoryError::Config(format!("{e:#}")))?;
        }
    }
    Ok(())
}

fn execute_okf(
    command: OkfCommands,
    conn: &Connection,
    cwd_project: Option<&str>,
) -> Result<(), MemoryError> {
    use crate::concepts::graph::{self, Direction, TraversalOptions};
    use agent_memory::okf::{BundleScope, OkfBundleHandler, OkfDocumentHandler};

    match command {
        OkfCommands::Validate { file } => {
            let text = read_text_path(&file)?;
            let handler = OkfDocumentHandler::new(conn, BundleScope::Unscoped);
            let parsed = handler.validate(&text).map_err(okf_cli_error)?;
            println!(
                "{}",
                render::render_action_result(
                    "okf_valid",
                    &[("diagnostics", parsed.diagnostics.len().to_string())],
                )
            );
        }
        OkfCommands::Get { target } => {
            let id = resolve_okf_target(conn, &target)?;
            let scope = okf_scope_for_id(conn, &id)?;
            let rendered = OkfDocumentHandler::new(conn, scope)
                .render(&id)
                .map_err(okf_cli_error)?;
            print!("{}", rendered.text);
        }
        OkfCommands::Put {
            target,
            file,
            expect_revision,
            dry_run,
        } => {
            let text = read_text_path(&file)?;
            let parsed = agent_memory::okf::parse_document(&text)
                .map_err(|error| MemoryError::Config(error.to_string()))?;
            let target_arg = (target != "new").then_some(target.as_str());
            let existing_id = if let Some(value) = target_arg {
                match resolve_okf_target(conn, value) {
                    Ok(id) => Some(id),
                    Err(MemoryError::NotFound(_)) => None,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            let scope = if let Some(id) = existing_id.as_deref() {
                okf_scope_for_id(conn, id)?
            } else {
                scope_from_concept(&parsed.concept, cwd_project)?
            };
            let handler = OkfDocumentHandler::new(conn, scope);
            let effective_target = existing_id.as_deref().or(target_arg);
            let result = handler
                .put(effective_target, &parsed, expect_revision, dry_run)
                .map_err(okf_cli_error)?;
            println!(
                "{}",
                render::render_action_result(
                    if dry_run {
                        "okf_put_dry_run"
                    } else {
                        "okf_put"
                    },
                    &[
                        ("id", result.id),
                        ("revision", result.revision.to_string()),
                        ("created", result.created.to_string()),
                        ("changed", result.changed.to_string()),
                        ("fields", result.diff.fields.len().to_string()),
                    ],
                )
            );
        }
        OkfCommands::Read { bundle, path } => {
            let scope = BundleScope::parse_uri(&bundle).map_err(okf_cli_error)?;
            let entry = OkfBundleHandler::new(conn, scope)
                .read(&path)
                .map_err(okf_cli_error)?;
            print!("{}", entry.content);
        }
        OkfCommands::List { bundle, path } => {
            let scope = BundleScope::parse_uri(&bundle).map_err(okf_cli_error)?;
            for entry in OkfBundleHandler::new(conn, scope)
                .list(&path)
                .map_err(okf_cli_error)?
            {
                println!(
                    "{}",
                    render::render_action_result(
                        "okf_entry",
                        &[
                            ("path", entry.path),
                            ("kind", format!("{:?}", entry.kind).to_ascii_lowercase()),
                            ("read_only", entry.read_only.to_string()),
                        ],
                    )
                );
            }
        }
        OkfCommands::Index {
            bundle,
            concept_type,
            tag,
        } => {
            let scope = BundleScope::parse_uri(&bundle).map_err(okf_cli_error)?;
            let entry = OkfBundleHandler::new(conn, scope)
                .index(concept_type.as_deref(), tag.as_deref())
                .map_err(okf_cli_error)?;
            print!("{}", entry.content);
        }
        OkfCommands::Log {
            bundle,
            cursor,
            limit,
        } => {
            let scope = BundleScope::parse_uri(&bundle).map_err(okf_cli_error)?;
            let page = OkfBundleHandler::new(conn, scope)
                .log(cursor, Some(limit))
                .map_err(okf_cli_error)?;
            print!("{}", page.document.content);
            if let Some(next) = page.next_cursor {
                println!("\n<!-- next-cursor: {next} -->");
            }
        }
        OkfCommands::History { id, limit } => {
            let id = resolve_okf_target(conn, &id)?;
            let mut stmt = conn.prepare(
                "SELECT revision, operation, actor, content_hash, created_at
                 FROM memory_revisions WHERE memory_id = ?1
                 ORDER BY revision DESC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![id, limit as i64], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (revision, operation, actor, hash, at) in rows {
                let mut attrs = vec![
                    ("id", id.clone()),
                    ("revision", revision.to_string()),
                    ("operation", operation),
                    ("hash", hash),
                    ("at", at),
                ];
                if let Some(actor) = actor {
                    attrs.push(("actor", actor));
                }
                println!("{}", render::render_action_result("okf_revision", &attrs));
            }
        }
        OkfCommands::Diff { id, rev_a, rev_b } => {
            let id = resolve_okf_target(conn, &id)?;
            let left = revision_snapshot(conn, &id, rev_a)?;
            let right = revision_snapshot(conn, &id, rev_b)?;
            let left = left.as_object().ok_or_else(|| {
                MemoryError::Config("revision snapshot is not an object".to_string())
            })?;
            let right = right.as_object().ok_or_else(|| {
                MemoryError::Config("revision snapshot is not an object".to_string())
            })?;
            let keys = left
                .keys()
                .chain(right.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            for field in keys {
                if left.get(&field) != right.get(&field) {
                    let before = bounded_value(left.get(&field).map(ToString::to_string));
                    let after = bounded_value(right.get(&field).map(ToString::to_string));
                    println!(
                        "{}",
                        render::render_action_result(
                            "okf_diff",
                            &[
                                ("id", id.clone()),
                                ("field", field),
                                ("before", before),
                                ("after", after),
                            ],
                        )
                    );
                }
            }
        }
        OkfCommands::Graph {
            target,
            relation,
            direction,
            depth,
            limit,
        } => {
            let id = resolve_okf_target(conn, &target)?;
            let direction = match direction {
                GraphDirection::In => Direction::Incoming,
                GraphDirection::Out => Direction::Outgoing,
                GraphDirection::Both => Direction::Both,
            };
            let relations = relation.into_iter().collect();
            let result = graph::traverse(
                conn,
                std::slice::from_ref(&id),
                &TraversalOptions {
                    direction,
                    relations,
                    max_depth: depth,
                    max_results: limit,
                    ..TraversalOptions::default()
                },
            )?;
            for path in result.paths {
                println!(
                    "{}",
                    render::render_action_result(
                        "okf_graph_path",
                        &[
                            ("root", id.clone()),
                            ("path", path.nodes.join(" -> ")),
                            ("depth", path.steps.len().to_string()),
                            ("cycle", path.cycle.to_string()),
                        ],
                    )
                );
            }
            for diagnostic in result.diagnostics {
                println!(
                    "{}",
                    render::render_action_result(
                        "okf_graph_unresolved",
                        &[
                            ("source", diagnostic.source),
                            ("reference", diagnostic.reference),
                            ("relation", diagnostic.relation),
                        ],
                    )
                );
            }
        }
        OkfCommands::Export {
            bundle,
            target,
            ids,
            dry_run,
        } => {
            let scope = BundleScope::parse_uri(&bundle).map_err(okf_cli_error)?;
            let result = agent_memory::okf::export_bundle(conn, scope, &target, &ids, dry_run)
                .map_err(okf_cli_error)?;
            println!(
                "{}",
                render::render_action_result(
                    if dry_run {
                        "okf_export_dry_run"
                    } else {
                        "okf_exported"
                    },
                    &[
                        ("target", target.display().to_string()),
                        ("files", result.files.len().to_string()),
                    ],
                )
            );
        }
        OkfCommands::Import {
            source,
            project,
            scope,
            dry_run,
        } => {
            let bundle_scope = match scope {
                Some(MemoryScope::Global) => BundleScope::Global,
                Some(MemoryScope::Project) | None => {
                    let project = project
                        .or_else(|| cwd_project.map(str::to_string))
                        .ok_or_else(|| {
                            MemoryError::Config(
                                "OKF import requires --project outside a project directory"
                                    .to_string(),
                            )
                        })?;
                    BundleScope::Project(project)
                }
            };
            let result = agent_memory::okf::import_bundle(conn, bundle_scope, &source, dry_run)
                .map_err(okf_cli_error)?;
            println!(
                "{}",
                render::render_action_result(
                    if dry_run {
                        "okf_import_dry_run"
                    } else {
                        "okf_imported"
                    },
                    &[
                        ("created", result.created.to_string()),
                        ("updated", result.updated.to_string()),
                        ("unchanged", result.unchanged.to_string()),
                    ],
                )
            );
        }
    }
    Ok(())
}

fn okf_cli_error(error: agent_memory::okf::HandlerError) -> MemoryError {
    MemoryError::Config(error.to_string())
}

fn read_text_path(path: &Path) -> Result<String, MemoryError> {
    if path == Path::new("-") {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        Ok(text)
    } else {
        std::fs::read_to_string(path).map_err(MemoryError::from)
    }
}

fn resolve_okf_target(conn: &Connection, target: &str) -> Result<String, MemoryError> {
    let raw = if let Some(path) = target.strip_prefix("/memories/") {
        path.strip_suffix(".md").unwrap_or(path)
    } else if target.starts_with("memory://") {
        target
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(target)
    } else {
        target
    };
    match queries::resolve_id_prefix(conn, raw)? {
        ResolvedId::Exact(id) => Ok(id),
        ResolvedId::Ambiguous(_) => {
            Err(MemoryError::Config(format!("ambiguous memory ID `{raw}`")))
        }
        ResolvedId::NotFound => Err(MemoryError::NotFound(raw.to_string())),
    }
}

fn okf_scope_for_id(
    conn: &Connection,
    id: &str,
) -> Result<agent_memory::okf::BundleScope, MemoryError> {
    let project = conn.query_row(
        "SELECT project FROM memories WHERE id = ?1",
        rusqlite::params![id],
        |row| row.get::<_, Option<String>>(0),
    )?;
    Ok(match project.as_deref() {
        Some(GLOBAL_PROJECT_IDENT) => agent_memory::okf::BundleScope::Global,
        Some(project) => agent_memory::okf::BundleScope::Project(project.to_string()),
        None => agent_memory::okf::BundleScope::Unscoped,
    })
}

fn scope_from_concept(
    concept: &agent_memory::okf::OkfConcept,
    cwd_project: Option<&str>,
) -> Result<agent_memory::okf::BundleScope, MemoryError> {
    let metadata = concept.agent_memory.as_ref();
    match metadata.and_then(|value| value.scope.as_deref()) {
        Some("global") => Ok(agent_memory::okf::BundleScope::Global),
        Some("unscoped") => Ok(agent_memory::okf::BundleScope::Unscoped),
        Some("project") | None => {
            if metadata.and_then(|value| value.project.as_deref()) == Some(GLOBAL_PROJECT_IDENT) {
                return Ok(agent_memory::okf::BundleScope::Global);
            }
            metadata
                .and_then(|value| value.project.clone())
                .or_else(|| cwd_project.map(str::to_string))
                .map(agent_memory::okf::BundleScope::Project)
                .ok_or_else(|| {
                    MemoryError::Config(
                        "new OKF concept requires project/scope metadata outside a project"
                            .to_string(),
                    )
                })
        }
        Some(scope) => Err(MemoryError::Config(format!(
            "unknown x-agent-memory scope `{scope}`"
        ))),
    }
}

fn revision_snapshot(
    conn: &Connection,
    id: &str,
    revision: i64,
) -> Result<serde_json::Value, MemoryError> {
    let json: String = conn
        .query_row(
            "SELECT snapshot_json FROM memory_revisions
             WHERE memory_id = ?1 AND revision = ?2",
            rusqlite::params![id, revision],
            |row| row.get(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => {
                MemoryError::NotFound(format!("{id}@{revision}"))
            }
            other => MemoryError::Database(other),
        })?;
    serde_json::from_str(&json).map_err(MemoryError::from)
}

fn bounded_value(value: Option<String>) -> String {
    let value = value.unwrap_or_else(|| "null".to_string());
    let mut chars = value.chars();
    let preview = chars.by_ref().take(240).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

/// Print a ranked result set (`search`/`context`) as grouped light-XML.
/// Delegates to the `render` module, appends the reflection hint, and emits
/// the `<usage>` legend at the bottom so a cold agent knows how to consume
/// the output (short-ID semantics, section meanings, how to fetch full
/// content). The legend ships unconditionally — even on zero-result runs —
/// because that's when a new caller most needs the guidance.
///
/// `query` is the original task/search string; it is inspected for
/// canonical-state intent (commit/tag/version/push/etc.) so the hint can
/// remind the agent to use git rather than memory for those facts.
fn print_ranked(
    results: &[SearchResult],
    boosts: &BoostConfig<'_>,
    query: &str,
    working_context: Option<&WorkingContext>,
    include_working_absence_hint: bool,
) {
    println!(
        "{}",
        render_ranked(
            results,
            boosts,
            query,
            working_context,
            include_working_absence_hint
        )
    );
}

/// Render a ranked result set (`search`/`context`) to a `String`, identical to
/// what [`print_ranked`] prints (sans the trailing newline `println!` adds).
///
/// Factored out so both the `Cli::Context` arm and the per-turn `hook::run`
/// path can reuse the exact same rendering — DRY: there is one code path for
/// turning ranked results + hints + working-context into the emitted block.
/// See [`print_ranked`] for the `query` canonical-state-intent semantics.
fn render_ranked(
    results: &[SearchResult],
    boosts: &BoostConfig<'_>,
    query: &str,
    working_context: Option<&WorkingContext>,
    include_working_absence_hint: bool,
) -> String {
    let total = results.len();
    let globals = results.iter().filter(|r| r.is_global).count();
    let cross = if boosts.current_project.is_some() {
        results.iter().filter(|r| !r.is_current_project).count()
    } else {
        0
    };
    let hint = combine_hints(
        results_hint(cross, globals, total, boosts.current_project, query),
        working_absence_hint(
            boosts.current_project,
            working_context,
            include_working_absence_hint,
        ),
    );
    let rendered = render::render_search_results(
        results,
        boosts.current_project,
        working_context,
        hint.as_deref(),
    );
    // Empty input yields an empty render; emit an explicit empty marker so
    // callers can tell "query ran, zero hits" from a silent failure.
    let body = if rendered.is_empty() {
        "<results count=\"0\"/>".to_string()
    } else {
        rendered
    };
    format!("{body}\n{}", render::render_usage_legend())
}

/// Run the `memory context` retrieval+render and RETURN the block as a String,
/// byte-for-byte equal to what `memory context` prints (sans the final
/// newline). Shared by the `Cli::Context` arm and the per-turn `hook::run`
/// path so there is no duplicated retrieval logic between them.
///
/// `no_working_context` mirrors the `--no-working-context` flag: when true the
/// WorkingContext fetch is skipped and its absence hint suppressed. Boost
/// resolution follows the same `--no-project-boost`-off defaults the CLI uses
/// (explicit project override, then cwd; global scope always on). `only`
/// mirrors `--only <project>`.
#[allow(clippy::too_many_arguments)]
pub fn retrieve_context_block(
    conn: &Connection,
    config: &Config,
    description: &str,
    limit: usize,
    no_working_context: bool,
    project: Option<&str>,
    cwd_project: Option<&str>,
    no_project_boost: bool,
    only: Option<&str>,
) -> Result<String, MemoryError> {
    let boosts = resolve_boosts(project, cwd_project, no_project_boost);
    let opts = SearchOptions {
        limit,
        current_project: boosts.current_project,
        boost_factor: boosts.project_boost,
        only_project: only,
        global_project: boosts.global_project,
        global_boost_factor: boosts.global_boost,
        concept_type: None,
        tag: None,
        graph_depth: 1,
    };
    let results = search::hybrid_search(conn, description, opts, &config.model_cache_dir)?;
    let working_context = if no_working_context {
        None
    } else if let Some(project) = boosts.current_project {
        queries::get_working_context(conn, project)?
    } else {
        None
    };
    Ok(render_ranked(
        &results,
        &boosts,
        description,
        working_context.as_ref(),
        !no_working_context,
    ))
}

fn combine_hints(primary: Option<String>, secondary: Option<String>) -> Option<String> {
    match (primary, secondary) {
        (Some(a), Some(b)) => Some(format!("{a} {b}")),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn working_absence_hint(
    current_project: Option<&str>,
    working_context: Option<&WorkingContext>,
    include: bool,
) -> Option<String> {
    if include && working_context.is_none() {
        current_project.map(|project| {
            format!(
                "No WorkingContext is set for project '{project}'. If pausing substantial active work, use `memory working set` to leave a handoff."
            )
        })
    } else {
        None
    }
}

/// Resolve a user-supplied `--id` argument for move/copy subcommands through
/// the short-prefix resolver. Ambiguous prefixes emit an `<ambiguous>` block
/// and return `Ok(None)` so the caller skips the mutation; missing IDs return
/// a `not_found` result line. Full UUIDs and unique prefixes return the full
/// UUID wrapped in `Some`.
fn resolve_id_arg(conn: &Connection, id: Option<&str>) -> Result<Option<String>, MemoryError> {
    match id {
        None => Ok(None),
        Some(raw) => match queries::resolve_id_prefix(conn, raw)? {
            ResolvedId::Exact(full) => Ok(Some(full)),
            ResolvedId::Ambiguous(cands) => {
                println!("{}", render::render_ambiguous(raw, &cands));
                // Sentinel: returning `None` here would let the caller fall
                // through to the `--from` branch. Instead we pass a token
                // that the mutation fns treat as "id was supplied but
                // unresolved" — achieved by returning an empty string, which
                // `run_move`/`run_copy` short-circuit on below.
                Ok(Some(String::new()))
            }
            ResolvedId::NotFound => {
                println!(
                    "{}",
                    render::render_action_result("not_found", &[("id", raw.to_string())])
                );
                Ok(Some(String::new()))
            }
        },
    }
}

/// Handle `memory update <id> --content ...` — the content-update form.
///
/// Resolves the ID prefix (short or full UUID), re-embeds the new content,
/// and calls `queries::update_content` which swaps `content`↔`content_raw`,
/// bumps `updated_at`, and clears `superseded_by`. Emits a light-XML
/// `<result status="updated" id="..."/>` line on success or the usual
/// `<ambiguous>` / `not_found` shape on resolution failures.
fn run_update_content(
    conn: &Connection,
    config: &Config,
    raw_id: &str,
    new_content: &str,
    tags_csv: Option<&str>,
    memory_type: Option<&str>,
) -> Result<(), MemoryError> {
    if new_content.trim().is_empty() {
        return Err(MemoryError::Config(
            "--content must not be empty".to_string(),
        ));
    }

    let full_id = match queries::resolve_id_prefix(conn, raw_id)? {
        ResolvedId::Exact(id) => id,
        ResolvedId::Ambiguous(cands) => {
            println!("{}", render::render_ambiguous(raw_id, &cands));
            return Ok(());
        }
        ResolvedId::NotFound => {
            println!(
                "{}",
                render::render_action_result("not_found", &[("id", raw_id.to_string())])
            );
            return Ok(());
        }
    };

    let tag_vec: Option<Vec<String>> = tags_csv.map(|t| {
        t.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    let changed =
        queries::update_content(conn, &full_id, new_content, tag_vec.as_deref(), memory_type)?;
    if !changed {
        // resolve_id_prefix returned Exact but the UPDATE touched zero rows —
        // implies a TOCTOU delete between resolve and update. Surface it
        // cleanly rather than claiming success.
        println!(
            "{}",
            render::render_action_result("not_found", &[("id", raw_id.to_string())])
        );
        return Ok(());
    }

    // Re-embed after the content swap so vector search picks up the new
    // form on the next query. We intentionally embed *after* the SQL UPDATE
    // so a flaky embedder doesn't block the metadata update — the worst
    // case is a stale embedding that the next dream pass corrects.
    let new_embedding = embedding::embed_text(new_content, &config.model_cache_dir)?;
    let blob = crate::db::models::embedding_to_blob(&new_embedding);
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE memories SET embedding = ?1, embedding_model = ?2, updated_at = ?3
         WHERE id = ?4",
        rusqlite::params![
            blob,
            crate::db::models::EMBEDDING_MODEL_NAME_DEFAULT,
            now,
            full_id
        ],
    )?;

    maybe_auto_push_after_memory_update(conn, config, &full_id);

    println!(
        "{}",
        render::render_action_result("updated", &[("id", render::short_id(&full_id).to_string())])
    );
    Ok(())
}

fn run_update_okf(
    conn: &Connection,
    config: &Config,
    raw_id: &str,
    file: &Path,
) -> Result<(), MemoryError> {
    let id = resolve_okf_target(conn, raw_id)?;
    let text = read_text_path(file)?;
    let parsed = agent_memory::okf::parse_document(&text)
        .map_err(|error| MemoryError::Config(error.to_string()))?;
    let embedding = embedding::embed_text(&parsed.concept.body, &config.model_cache_dir)?;
    let expected = parsed
        .concept
        .agent_memory
        .as_ref()
        .and_then(|metadata| metadata.revision)
        .and_then(|revision| i64::try_from(revision).ok());
    let scope = okf_scope_for_id(conn, &id)?;
    let result = agent_memory::okf::OkfDocumentHandler::new(conn, scope)
        .put(Some(&id), &parsed, expected, false)
        .map_err(okf_cli_error)?;
    let blob = embedding_to_blob(&embedding);
    conn.execute(
        "UPDATE memories SET embedding = ?1, embedding_model = ?2 WHERE id = ?3",
        rusqlite::params![blob, EMBEDDING_MODEL_NAME_DEFAULT, id],
    )?;
    maybe_auto_push_after_memory_update(conn, config, &id);
    println!(
        "{}",
        render::render_action_result(
            "okf_updated",
            &[
                ("id", id),
                ("revision", result.revision.to_string()),
                ("changed", result.changed.to_string()),
            ],
        )
    );
    Ok(())
}

#[derive(Debug)]
struct PushCandidate {
    local_memory_id: String,
    gateway_memory: GatewayMemory,
    action: &'static str,
}

#[derive(Debug)]
struct AutoPushSummary {
    candidates: usize,
    pending: usize,
    skipped: usize,
    counts: PushResponseCounts,
}

#[derive(Debug)]
struct AutoPullSummary {
    remote: usize,
    conflicts: usize,
    cursor_updated: bool,
}

#[derive(Debug)]
struct PullPlan {
    action: &'static str,
    remote: GatewayMemory,
    local_memory_id: Option<String>,
    local_content_hash: Option<String>,
}

#[derive(Debug, Default)]
struct PullProjectSummary {
    remote: usize,
    conflicts: usize,
    cursors_updated: usize,
}

impl PullProjectSummary {
    fn add_assign(&mut self, other: &PullProjectSummary) {
        self.remote += other.remote;
        self.conflicts += other.conflicts;
        self.cursors_updated += other.cursors_updated;
    }
}

fn maybe_auto_sync_after_store(conn: &Connection, config: &Config, memory: &Memory) {
    let Some(project) = auto_sync_project_for_memory(config, memory) else {
        return;
    };

    match run_auto_sync_after_store(conn, config, project) {
        Ok((push, pull)) => print_auto_sync_complete(project, &push, &pull),
        Err(err) => println!(
            "{}",
            render::render_hint(&format!(
                "gateway auto-sync skipped after store: {err}. The memory was saved locally; run `memory push` or `memory pull` to retry manually."
            ))
        ),
    }
}

fn maybe_auto_push_after_memory_update(conn: &Connection, config: &Config, memory_id: &str) {
    let memory = match queries::get_memory_by_id(conn, memory_id) {
        Ok(memory) => memory,
        Err(err) => {
            println!(
                "{}",
                render::render_hint(&format!(
                    "gateway auto-sync skipped after update: {err}. The memory was updated locally; run `memory push` to retry manually."
                ))
            );
            return;
        }
    };

    match crate::gateway_sync::push_memory_update_if_configured(conn, &config.gateway, &memory) {
        Ok(outcome) if outcome.was_synced() => {
            println!(
                "{}",
                render::render_action_result(
                    "gateway_auto_sync",
                    &[
                        ("operation", "update".to_string()),
                        ("id", render::short_id(memory_id).to_string()),
                    ],
                )
            );
        }
        Ok(_) => {}
        Err(err) => println!(
            "{}",
            render::render_hint(&format!(
                "gateway auto-sync skipped after update: {err}. The memory was updated locally; run `memory push` to retry manually."
            ))
        ),
    }
}

fn auto_sync_project_for_memory<'a>(config: &Config, memory: &'a Memory) -> Option<&'a str> {
    if !config.gateway.auto_sync_enabled() {
        return None;
    }
    match memory.project.as_deref() {
        Some(project) if project != GLOBAL_PROJECT_IDENT => Some(project),
        _ => None,
    }
}

fn run_auto_sync_after_store(
    conn: &Connection,
    config: &Config,
    project: &str,
) -> Result<(AutoPushSummary, AutoPullSummary), MemoryError> {
    let push = run_auto_push(conn, config, project, DEFAULT_GATEWAY_PUSH_BATCH_SIZE)?;
    let pull = run_auto_pull(conn, config, project)?;
    Ok((push, pull))
}

fn run_auto_push(
    conn: &Connection,
    config: &Config,
    project: &str,
    batch_size: usize,
) -> Result<AutoPushSummary, MemoryError> {
    let candidates =
        build_push_candidates_with_okf(conn, project, config.gateway.okf_sync_enabled())?;
    let pending: Vec<&PushCandidate> = candidates
        .iter()
        .filter(|candidate| candidate.action != "skipped")
        .collect();
    let pending_count = pending.len();
    let skipped = candidates.len().saturating_sub(pending_count);
    let batch_size = validate_push_batch_size(batch_size)?;
    let mut counts = PushResponseCounts::default();

    if !pending.is_empty() {
        let gateway =
            MemoryGatewayClient::from_config(&config.gateway).map_err(map_gateway_error)?;
        for batch in push_batches(&pending, batch_size) {
            let (request, hashes) = build_push_request(project, batch);
            let response = gateway.push_memories(&request).map_err(map_gateway_error)?;
            apply_push_response(conn, project, &response, &hashes)?;
            for result in &response.results {
                counts.record(&result.action);
            }
        }
    }

    Ok(AutoPushSummary {
        candidates: candidates.len(),
        pending: pending_count,
        skipped,
        counts,
    })
}

fn run_auto_pull(
    conn: &Connection,
    config: &Config,
    project: &str,
) -> Result<AutoPullSummary, MemoryError> {
    let state = queries::get_project_gateway_sync_state(conn, project)?;
    let request = PullMemoriesRequest {
        project: project.to_string(),
        since_server_revision: state.as_ref().and_then(|s| s.last_pull_server_revision),
        cursor: state.and_then(|s| s.last_pull_cursor),
        known_memories: Vec::new(),
        limit: Some(100),
    };
    let response = MemoryGatewayClient::from_config(&config.gateway)
        .map_err(map_gateway_error)?
        .pull_memories(&request)
        .map_err(map_gateway_error)?;
    response.validate_project_scope().map_err(|err| {
        MemoryError::Config(format!(
            "gateway pull response failed scope validation: {err}"
        ))
    })?;

    let plans = plan_pull_actions(conn, project, &response)?;
    let conflicts = apply_pull_plans(conn, config, project, &plans)?;
    let cursor_updated = conflicts == 0;
    if cursor_updated {
        queries::upsert_project_gateway_sync_state(
            conn,
            project,
            response.server_revision,
            response.next_cursor.as_deref(),
        )?;
    }
    Ok(AutoPullSummary {
        remote: plans.len(),
        conflicts,
        cursor_updated,
    })
}

fn print_auto_sync_complete(project: &str, push: &AutoPushSummary, pull: &AutoPullSummary) {
    println!(
        "{}",
        render::render_action_result(
            "gateway_auto_sync",
            &[
                ("project", project.to_string()),
                ("push_candidates", push.candidates.to_string()),
                ("push_pending", push.pending.to_string()),
                ("push_skipped", push.skipped.to_string()),
                ("push_created", push.counts.created.to_string()),
                ("push_updated", push.counts.updated.to_string()),
                ("push_linked", push.counts.linked.to_string()),
                ("push_deleted", push.counts.deleted.to_string()),
                ("push_conflicts", push.counts.conflicts.to_string()),
                ("push_rejected", push.counts.rejected.to_string()),
                ("pull_remote", pull.remote.to_string()),
                ("pull_conflicts", pull.conflicts.to_string()),
                ("cursor_updated", pull.cursor_updated.to_string()),
            ],
        )
    );
}

fn run_push(
    conn: &Connection,
    config: &Config,
    cwd_project: Option<&str>,
    status_only: bool,
    batch_size: usize,
    all: bool,
) -> Result<(), MemoryError> {
    let batch_size = validate_push_batch_size(batch_size)?;
    if all {
        return run_push_all(conn, config, status_only, batch_size);
    }
    let project = resolve_gateway_project(cwd_project)?;
    let gateway = if status_only {
        None
    } else {
        Some(MemoryGatewayClient::from_config(&config.gateway).map_err(map_gateway_error)?)
    };
    run_push_project(conn, gateway.as_ref(), &project, status_only, batch_size)?;
    Ok(())
}

fn run_push_all(
    conn: &Connection,
    config: &Config,
    status_only: bool,
    batch_size: usize,
) -> Result<(), MemoryError> {
    let projects = list_local_gateway_projects(conn)?;
    let gateway = if status_only || projects.is_empty() {
        None
    } else {
        Some(MemoryGatewayClient::from_config(&config.gateway).map_err(map_gateway_error)?)
    };
    let mut total = PushProjectSummary::default();
    for project in &projects {
        let summary = run_push_project(conn, gateway.as_ref(), project, status_only, batch_size)?;
        total.add_assign(&summary);
    }
    println!(
        "{}",
        render::render_action_result(
            if status_only {
                "push_all_status"
            } else {
                "push_all_complete"
            },
            &[
                ("projects", projects.len().to_string()),
                ("candidates", total.candidates.to_string()),
                ("pending", total.pending.to_string()),
                ("skipped", total.skipped.to_string()),
                ("created", total.counts.created.to_string()),
                ("updated", total.counts.updated.to_string()),
                ("linked", total.counts.linked.to_string()),
                ("deleted", total.counts.deleted.to_string()),
                ("conflicts", total.counts.conflicts.to_string()),
                ("rejected", total.counts.rejected.to_string()),
            ],
        )
    );
    Ok(())
}

fn run_push_project(
    conn: &Connection,
    gateway: Option<&MemoryGatewayClient>,
    project: &str,
    status_only: bool,
    batch_size: usize,
) -> Result<PushProjectSummary, MemoryError> {
    let candidates = build_push_candidates_with_okf(
        conn,
        project,
        gateway.is_some_and(MemoryGatewayClient::okf_enabled),
    )?;
    let pending: Vec<&PushCandidate> = candidates
        .iter()
        .filter(|candidate| candidate.action != "skipped")
        .collect();
    let skipped = candidates.len().saturating_sub(pending.len());

    print_push_status(project, &candidates);
    if status_only {
        return Ok(PushProjectSummary {
            candidates: candidates.len(),
            pending: pending.len(),
            skipped,
            counts: PushResponseCounts::default(),
        });
    }

    if pending.is_empty() {
        println!(
            "{}",
            render::render_action_result("push_complete", &[("sent", "0".to_string())])
        );
        return Ok(PushProjectSummary {
            candidates: candidates.len(),
            pending: 0,
            skipped,
            counts: PushResponseCounts::default(),
        });
    }

    let gateway = gateway.ok_or_else(|| {
        MemoryError::Config("memory push requires configured gateway client".to_string())
    })?;
    let batches = push_batches(&pending, batch_size);
    let total_batches = batches.len();
    let mut counts = PushResponseCounts::default();

    for (batch_index, batch) in batches.into_iter().enumerate() {
        if total_batches > 1 {
            print_push_batch(batch_index + 1, total_batches, batch.len());
        }
        let (request, hashes) = build_push_request(project, batch);
        let response = gateway.push_memories(&request).map_err(map_gateway_error)?;
        apply_push_response(conn, project, &response, &hashes)?;
        print_push_results(&response, &mut counts);
    }

    print_push_complete(project, &counts);
    Ok(PushProjectSummary {
        candidates: candidates.len(),
        pending: pending.len(),
        skipped,
        counts,
    })
}

fn validate_push_batch_size(batch_size: usize) -> Result<usize, MemoryError> {
    if batch_size == 0 {
        return Err(MemoryError::Config(
            "memory push --batch-size must be at least 1".to_string(),
        ));
    }
    if batch_size > MAX_GATEWAY_PUSH_BATCH_SIZE {
        return Err(MemoryError::Config(format!(
            "memory push --batch-size must be no greater than {MAX_GATEWAY_PUSH_BATCH_SIZE}"
        )));
    }
    Ok(batch_size)
}

fn list_local_gateway_projects(conn: &Connection) -> Result<Vec<String>, MemoryError> {
    let mut projects: Vec<String> = queries::list_projects(conn)?
        .into_iter()
        .filter_map(|(project, _)| project)
        .filter(|project| !project.trim().is_empty())
        .collect();
    projects.extend(queries::list_memory_gateway_sync_delete_pending_projects(
        conn,
    )?);
    projects.sort();
    projects.dedup();
    Ok(projects)
}

fn push_batches<'a>(
    pending: &'a [&'a PushCandidate],
    batch_size: usize,
) -> Vec<&'a [&'a PushCandidate]> {
    pending.chunks(batch_size).collect()
}

fn build_push_request(
    project: &str,
    candidates: &[&PushCandidate],
) -> (PushMemoriesRequest, HashMap<String, String>) {
    let request = PushMemoriesRequest {
        project: project.to_string(),
        memories: candidates
            .iter()
            .map(|candidate| candidate.gateway_memory.clone())
            .collect(),
    };
    let hashes = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.local_memory_id.clone(),
                candidate.gateway_memory.content_hash.clone(),
            )
        })
        .collect();
    (request, hashes)
}

#[cfg(test)]
fn build_push_candidates(
    conn: &Connection,
    project: &str,
) -> Result<Vec<PushCandidate>, MemoryError> {
    build_push_candidates_with_okf(conn, project, false)
}

fn build_push_candidates_with_okf(
    conn: &Connection,
    project: &str,
    okf_enabled: bool,
) -> Result<Vec<PushCandidate>, MemoryError> {
    let memories = queries::list_memories_by_project(conn, Some(project))?;
    let mut memory_syncs = Vec::new();
    let mut synced_by_hash: HashMap<String, MemoryGatewaySync> = HashMap::new();
    for memory in memories {
        let sync = queries::get_memory_gateway_sync(conn, &memory.id)?;
        if let Some(record) = sync.as_ref() {
            if !record.tombstone_deleted {
                if let Some(hash) = sync_content_hash(record) {
                    synced_by_hash
                        .entry(hash.to_string())
                        .or_insert_with(|| record.clone());
                }
            }
        }
        memory_syncs.push((memory, sync));
    }

    let mut candidates = Vec::new();
    for (memory, sync) in memory_syncs {
        let gateway_memory =
            gateway_memory_from_local(conn, &memory, project, sync.as_ref(), okf_enabled)?;
        let duplicate_sync = if sync.is_none() {
            synced_by_hash.get(&gateway_memory.content_hash)
        } else {
            None
        };
        let action = match (sync.as_ref(), duplicate_sync) {
            (Some(record), _) if sync_record_matches_hash(record, &gateway_memory.content_hash) => {
                "skipped"
            }
            (Some(_), _) => "update",
            (None, Some(record)) => {
                let gateway_memory =
                    gateway_memory_from_local(conn, &memory, project, Some(record), okf_enabled)?;
                candidates.push(PushCandidate {
                    local_memory_id: memory.id,
                    gateway_memory,
                    action: "skipped",
                });
                continue;
            }
            (None, None) => "create",
        };
        candidates.push(PushCandidate {
            local_memory_id: memory.id,
            gateway_memory,
            action,
        });
    }
    for sync in queries::list_memory_gateway_sync_delete_pending_by_project(conn, project)? {
        candidates.push(PushCandidate {
            local_memory_id: sync.local_memory_id.clone(),
            gateway_memory: crate::gateway_sync::gateway_tombstone_from_sync(
                &sync,
                "memory deleted locally",
            ),
            action: "delete",
        });
    }
    Ok(candidates)
}

fn sync_content_hash(record: &MemoryGatewaySync) -> Option<&str> {
    record
        .last_pushed_content_hash
        .as_deref()
        .or(record.last_pulled_content_hash.as_deref())
}

fn sync_record_matches_hash(record: &MemoryGatewaySync, content_hash: &str) -> bool {
    record.last_pushed_content_hash.as_deref() == Some(content_hash)
        || record.last_pulled_content_hash.as_deref() == Some(content_hash)
}

fn gateway_memory_from_local(
    conn: &Connection,
    memory: &Memory,
    project: &str,
    sync: Option<&MemoryGatewaySync>,
    okf_enabled: bool,
) -> Result<GatewayMemory, MemoryError> {
    if memory.project.as_deref() != Some(project) {
        return Err(MemoryError::Config(format!(
            "memory {} is not scoped to project {project}",
            memory.id
        )));
    }
    let memory_type = memory
        .memory_type
        .clone()
        .unwrap_or_else(|| "user".to_string());
    let tags = memory.tags.clone().unwrap_or_default();
    let content_hash = memory_content_hash(&memory.content, &memory_type, &tags);
    let okf = okf_enabled
        .then(|| gateway_okf_envelope(conn, memory))
        .transpose()?;
    let concept_hash = okf.as_ref().map(|envelope| envelope.semantic_hash.clone());
    Ok(GatewayMemory {
        project: project.to_string(),
        content: memory.content.clone(),
        memory_type,
        tags,
        content_hash,
        concept_hash,
        okf,
        local_memory_id: Some(memory.id.clone()),
        client_id: Some(memory.id.clone()),
        gateway_memory_id: sync.map(|record| record.gateway_memory_id.clone()),
        base_server_revision: sync.map(|record| record.last_seen_server_revision),
        server_revision: None,
        created_at: Some(memory.created_at.clone()),
        updated_at: Some(memory.updated_at.clone()),
        provenance: Some(gateway_provenance_from_local(memory)),
        tombstone: None,
    })
}

fn gateway_provenance_from_local(memory: &Memory) -> GatewayMemoryProvenance {
    GatewayMemoryProvenance {
        source_agent_id: memory.agent.clone(),
        source_machine_id: local_host_alias(),
        source_os: Some(env::consts::OS.to_string()),
        source_arch: Some(env::consts::ARCH.to_string()),
        source_system: Some("agent-memory".to_string()),
        pushed_at: Some(chrono::Utc::now().to_rfc3339()),
    }
}

fn local_host_alias() -> Option<String> {
    ["AGENT_MEMORY_HOST", "HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|key| {
            env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn apply_push_response(
    conn: &Connection,
    project: &str,
    response: &PushMemoriesResponse,
    hashes_by_local_id: &HashMap<String, String>,
) -> Result<(), MemoryError> {
    for result in &response.results {
        let Some(local_memory_id) = result.local_memory_id.as_deref() else {
            continue;
        };
        let existing_local_sync = queries::get_memory_gateway_sync(conn, local_memory_id)?;
        let queued_delete = queries::get_memory_gateway_delete_pending(conn, local_memory_id)?;
        let pending_delete = queued_delete.is_some()
            || existing_local_sync.as_ref().is_some_and(|record| {
                record.tombstone_deleted && record.sync_state == "delete_pending"
            });
        if matches!(
            result.action,
            PushMemoryAction::Deleted | PushMemoryAction::Tombstoned
        ) {
            queries::clear_memory_gateway_sync(conn, local_memory_id)?;
            queries::clear_memory_gateway_delete_pending(conn, local_memory_id)?;
            continue;
        }
        if pending_delete && matches!(result.action, PushMemoryAction::Updated) {
            queries::clear_memory_gateway_sync(conn, local_memory_id)?;
            queries::clear_memory_gateway_delete_pending(conn, local_memory_id)?;
            continue;
        }
        if pending_delete
            && matches!(
                result.action,
                PushMemoryAction::Created | PushMemoryAction::Linked
            )
        {
            return Err(MemoryError::Config(format!(
                "gateway push returned unexpected action {:?} for pending delete {local_memory_id}",
                result.action
            )));
        }

        let should_record = matches!(
            result.action,
            PushMemoryAction::Created | PushMemoryAction::Updated | PushMemoryAction::Linked
        );
        if !should_record {
            continue;
        }
        let Some(gateway_memory_id) = result.gateway_memory_id.as_deref() else {
            continue;
        };
        let Some(server_revision) = result.server_revision else {
            continue;
        };
        let content_hash = result
            .content_hash
            .clone()
            .or_else(|| hashes_by_local_id.get(local_memory_id).cloned());

        if let Some(existing) =
            queries::get_memory_gateway_sync_by_gateway_id(conn, project, gateway_memory_id)?
        {
            if existing.local_memory_id != local_memory_id {
                let existing_matches = content_hash
                    .as_deref()
                    .map(|hash| sync_record_matches_hash(&existing, hash))
                    .unwrap_or(false);
                if matches!(result.action, PushMemoryAction::Linked) || existing_matches {
                    continue;
                }
                return Err(MemoryError::Config(format!(
                    "gateway push response mapped local memory {local_memory_id} to gateway memory {gateway_memory_id}, but that gateway memory is already linked to local memory {}",
                    existing.local_memory_id
                )));
            }
        }

        queries::upsert_memory_gateway_sync(
            conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: local_memory_id.to_string(),
                project: project.to_string(),
                gateway_memory_id: gateway_memory_id.to_string(),
                last_seen_server_revision: server_revision,
                last_pushed_content_hash: content_hash,
                last_pulled_content_hash: None,
                sync_state: format!("{:?}", result.action).to_ascii_lowercase(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )?;
    }
    Ok(())
}

fn print_push_status(project: &str, candidates: &[PushCandidate]) {
    let pending = candidates
        .iter()
        .filter(|candidate| candidate.action != "skipped")
        .count();
    let skipped = candidates.len().saturating_sub(pending);
    println!(
        "{}",
        render::render_action_result(
            "push_status",
            &[
                ("project", project.to_string()),
                ("candidates", candidates.len().to_string()),
                ("pending", pending.to_string()),
                ("skipped", skipped.to_string()),
            ],
        )
    );
    for candidate in candidates {
        let mut attrs = vec![
            (
                "id",
                render::short_id(&candidate.local_memory_id).to_string(),
            ),
            ("action", candidate.action.to_string()),
            ("hash", candidate.gateway_memory.content_hash.clone()),
            ("type", candidate.gateway_memory.memory_type.clone()),
        ];
        if let Some(gateway_id) = candidate.gateway_memory.gateway_memory_id.as_deref() {
            attrs.push(("gateway_id", gateway_id.to_string()));
        }
        if let Some(rev) = candidate.gateway_memory.base_server_revision {
            attrs.push(("base_revision", rev.to_string()));
        }
        println!("{}", render::render_action_result("push_candidate", &attrs));
    }
}

#[derive(Debug, Default)]
struct PushProjectSummary {
    candidates: usize,
    pending: usize,
    skipped: usize,
    counts: PushResponseCounts,
}

impl PushProjectSummary {
    fn add_assign(&mut self, other: &PushProjectSummary) {
        self.candidates += other.candidates;
        self.pending += other.pending;
        self.skipped += other.skipped;
        self.counts.add_assign(&other.counts);
    }
}

#[derive(Debug, Default)]
struct PushResponseCounts {
    created: usize,
    updated: usize,
    linked: usize,
    deleted: usize,
    conflicts: usize,
    rejected: usize,
}

impl PushResponseCounts {
    fn record(&mut self, action: &PushMemoryAction) {
        match action {
            PushMemoryAction::Created => self.created += 1,
            PushMemoryAction::Updated => self.updated += 1,
            PushMemoryAction::Linked => self.linked += 1,
            PushMemoryAction::Deleted | PushMemoryAction::Tombstoned => self.deleted += 1,
            PushMemoryAction::Conflict => self.conflicts += 1,
            PushMemoryAction::Rejected => self.rejected += 1,
        }
    }

    fn add_assign(&mut self, other: &PushResponseCounts) {
        self.created += other.created;
        self.updated += other.updated;
        self.linked += other.linked;
        self.deleted += other.deleted;
        self.conflicts += other.conflicts;
        self.rejected += other.rejected;
    }
}

fn print_push_batch(index: usize, total: usize, records: usize) {
    println!(
        "{}",
        render::render_action_result(
            "push_batch",
            &[
                ("index", index.to_string()),
                ("total", total.to_string()),
                ("records", records.to_string()),
            ],
        )
    );
}

fn print_push_results(response: &PushMemoriesResponse, counts: &mut PushResponseCounts) {
    for result in &response.results {
        let action = push_action_label(&result.action);
        counts.record(&result.action);
        let mut attrs = vec![("action", action.to_string())];
        if let Some(id) = result.local_memory_id.as_deref() {
            attrs.push(("id", render::short_id(id).to_string()));
        }
        if let Some(gateway_id) = result.gateway_memory_id.as_deref() {
            attrs.push(("gateway_id", gateway_id.to_string()));
        }
        if let Some(rev) = result.server_revision {
            attrs.push(("server_revision", rev.to_string()));
        }
        if let Some(conflict) = result.conflict.as_ref() {
            attrs.push(("reason", conflict.reason.clone()));
        }
        if let Some(error) = result.error.as_deref() {
            attrs.push(("error", error.to_string()));
        }
        if let Some(error) = result.errors.first() {
            attrs.push(("error", error.code.clone()));
        }
        println!("{}", render::render_action_result("push_result", &attrs));
    }
}

fn print_push_complete(project: &str, counts: &PushResponseCounts) {
    println!(
        "{}",
        render::render_action_result(
            "push_complete",
            &[
                ("project", project.to_string()),
                ("created", counts.created.to_string()),
                ("updated", counts.updated.to_string()),
                ("linked", counts.linked.to_string()),
                ("deleted", counts.deleted.to_string()),
                ("conflicts", counts.conflicts.to_string()),
                ("rejected", counts.rejected.to_string()),
            ],
        )
    );
}

fn push_action_label(action: &PushMemoryAction) -> &'static str {
    match action {
        PushMemoryAction::Created => "created",
        PushMemoryAction::Updated => "updated",
        PushMemoryAction::Linked => "linked",
        PushMemoryAction::Deleted => "deleted",
        PushMemoryAction::Tombstoned => "tombstoned",
        PushMemoryAction::Conflict => "conflict",
        PushMemoryAction::Rejected => "rejected",
    }
}

fn run_pull(
    conn: &Connection,
    config: &Config,
    cwd_project: Option<&str>,
    status_only: bool,
    all: bool,
) -> Result<(), MemoryError> {
    let gateway = MemoryGatewayClient::from_config(&config.gateway).map_err(map_gateway_error)?;
    if all {
        return run_pull_all(conn, config, &gateway, status_only);
    }
    let project = resolve_gateway_project(cwd_project)?;
    run_pull_project(conn, config, &gateway, &project, status_only)?;
    Ok(())
}

fn run_pull_all(
    conn: &Connection,
    config: &Config,
    gateway: &MemoryGatewayClient,
    status_only: bool,
) -> Result<(), MemoryError> {
    let response = gateway.list_memory_projects().map_err(map_gateway_error)?;
    let mut projects: Vec<String> = response
        .project_idents()
        .filter(|project| !project.trim().is_empty())
        .map(str::to_string)
        .collect();
    projects.sort();
    projects.dedup();

    let mut total = PullProjectSummary::default();
    for project in &projects {
        let summary = run_pull_project(conn, config, gateway, project, status_only)?;
        total.add_assign(&summary);
    }
    println!(
        "{}",
        render::render_action_result(
            if status_only {
                "pull_all_status"
            } else {
                "pull_all_complete"
            },
            &[
                ("projects", projects.len().to_string()),
                ("remote", total.remote.to_string()),
                ("conflicts", total.conflicts.to_string()),
                ("cursors_updated", total.cursors_updated.to_string(),),
            ],
        )
    );
    Ok(())
}

fn run_pull_project(
    conn: &Connection,
    config: &Config,
    gateway: &MemoryGatewayClient,
    project: &str,
    status_only: bool,
) -> Result<PullProjectSummary, MemoryError> {
    let state = queries::get_project_gateway_sync_state(conn, project)?;
    let request = PullMemoriesRequest {
        project: project.to_string(),
        since_server_revision: state.as_ref().and_then(|s| s.last_pull_server_revision),
        cursor: state.and_then(|s| s.last_pull_cursor),
        known_memories: Vec::new(),
        limit: Some(100),
    };
    let response = gateway.pull_memories(&request).map_err(map_gateway_error)?;
    response.validate_project_scope().map_err(|err| {
        MemoryError::Config(format!(
            "gateway pull response failed scope validation: {err}"
        ))
    })?;

    let plans = plan_pull_actions(conn, project, &response)?;
    print_pull_status(project, &plans, status_only);
    if status_only {
        return Ok(PullProjectSummary {
            remote: plans.len(),
            conflicts: plans
                .iter()
                .filter(|plan| plan.action == "conflict")
                .count(),
            cursors_updated: 0,
        });
    }

    let conflicts = apply_pull_plans(conn, config, project, &plans)?;
    let cursor_updated = conflicts == 0;
    if conflicts == 0 {
        queries::upsert_project_gateway_sync_state(
            conn,
            project,
            response.server_revision,
            response.next_cursor.as_deref(),
        )?;
    }
    println!(
        "{}",
        render::render_action_result(
            "pull_complete",
            &[
                ("project", project.to_string()),
                ("conflicts", conflicts.to_string()),
                ("cursor_updated", cursor_updated.to_string()),
            ],
        )
    );
    Ok(PullProjectSummary {
        remote: plans.len(),
        conflicts,
        cursors_updated: usize::from(cursor_updated),
    })
}

fn plan_pull_actions(
    conn: &Connection,
    project: &str,
    response: &PullMemoriesResponse,
) -> Result<Vec<PullPlan>, MemoryError> {
    let local_memories = queries::list_memories_by_project(conn, Some(project))?;
    let mut local_hashes: HashMap<String, String> = HashMap::new();
    for memory in &local_memories {
        local_hashes.insert(memory.id.clone(), local_memory_hash(memory));
    }

    let mut plans = Vec::new();
    for remote in &response.memories {
        let gateway_id = remote.gateway_memory_id.as_deref();
        let existing = match gateway_id {
            Some(id) => queries::get_memory_gateway_sync_by_gateway_id(conn, project, id)?,
            None => None,
        };

        if remote
            .tombstone
            .as_ref()
            .map(|t| t.deleted)
            .unwrap_or(false)
        {
            plans.push(PullPlan {
                action: if existing.is_some() {
                    "tombstone"
                } else {
                    "tombstone_unknown"
                },
                remote: remote.clone(),
                local_memory_id: existing.map(|record| record.local_memory_id),
                local_content_hash: None,
            });
            continue;
        }

        let Some(remote_revision) = remote.server_revision else {
            plans.push(PullPlan {
                action: "rejected",
                remote: remote.clone(),
                local_memory_id: None,
                local_content_hash: None,
            });
            continue;
        };
        if gateway_id.is_none() {
            plans.push(PullPlan {
                action: "rejected",
                remote: remote.clone(),
                local_memory_id: None,
                local_content_hash: None,
            });
            continue;
        }

        if let Some(record) = existing {
            let local_hash = local_hashes.get(&record.local_memory_id).cloned();
            let base_hash = record
                .last_pulled_content_hash
                .as_ref()
                .or(record.last_pushed_content_hash.as_ref());
            let action = if record.last_seen_server_revision >= remote_revision {
                "skipped"
            } else if local_hash.as_ref() == base_hash {
                "update"
            } else {
                "conflict"
            };
            plans.push(PullPlan {
                action,
                remote: remote.clone(),
                local_memory_id: Some(record.local_memory_id),
                local_content_hash: local_hash,
            });
            continue;
        }

        let exact_local = local_hashes
            .iter()
            .find(|(_, hash)| *hash == &remote.content_hash)
            .map(|(id, hash)| (id.clone(), hash.clone()));
        match exact_local {
            Some((id, hash)) => plans.push(PullPlan {
                action: "link",
                remote: remote.clone(),
                local_memory_id: Some(id),
                local_content_hash: Some(hash),
            }),
            None => plans.push(PullPlan {
                action: "import",
                remote: remote.clone(),
                local_memory_id: None,
                local_content_hash: None,
            }),
        }
    }
    Ok(plans)
}

fn apply_pull_plans(
    conn: &Connection,
    config: &Config,
    project: &str,
    plans: &[PullPlan],
) -> Result<usize, MemoryError> {
    let mut conflicts = 0usize;
    for plan in plans {
        match plan.action {
            "import" => {
                let local_id = insert_remote_memory(conn, config, project, &plan.remote)?;
                upsert_pull_sync(conn, project, &local_id, &plan.remote, false, None)?;
            }
            "link" => {
                if let Some(local_id) = plan.local_memory_id.as_deref() {
                    upsert_pull_sync(conn, project, local_id, &plan.remote, false, None)?;
                }
            }
            "update" => {
                if let Some(local_id) = plan.local_memory_id.as_deref() {
                    if plan.remote.okf.is_some() {
                        apply_remote_okf(conn, config, project, &plan.remote, Some(local_id))?;
                    } else {
                        let tags = plan.remote.tags.clone();
                        queries::update_content(
                            conn,
                            local_id,
                            &plan.remote.content,
                            Some(&tags),
                            Some(&plan.remote.memory_type),
                        )?;
                        reembed_memory(conn, config, local_id, &plan.remote.content)?;
                    }
                    upsert_pull_sync(conn, project, local_id, &plan.remote, false, None)?;
                }
            }
            "tombstone" => {
                if let Some(local_id) = plan.local_memory_id.as_deref() {
                    upsert_pull_sync(
                        conn,
                        project,
                        local_id,
                        &plan.remote,
                        true,
                        plan.remote.tombstone.as_ref(),
                    )?;
                }
            }
            "conflict" => conflicts += 1,
            _ => {}
        }
    }
    Ok(conflicts)
}

fn insert_remote_memory(
    conn: &Connection,
    config: &Config,
    project: &str,
    remote: &GatewayMemory,
) -> Result<String, MemoryError> {
    if remote.okf.is_some() {
        return apply_remote_okf(conn, config, project, remote, None);
    }
    let mut memory = Memory::new(
        remote.content.clone(),
        Some(remote.tags.clone()),
        Some(project.to_string()),
        remote
            .provenance
            .as_ref()
            .and_then(|p| p.source_agent_id.clone()),
        remote
            .gateway_memory_id
            .as_ref()
            .map(|id| format!("gateway:{id}")),
        Some(remote.memory_type.clone()),
    );
    if let Some(created_at) = remote.created_at.as_ref() {
        memory.created_at = created_at.clone();
    }
    if let Some(updated_at) = remote.updated_at.as_ref() {
        memory.updated_at = updated_at.clone();
    }
    memory.embedding = Some(embedding::embed_text(
        &remote.content,
        &config.model_cache_dir,
    )?);
    crate::concepts::insert_memory(conn, &memory, "gateway_pull", memory.agent.as_deref(), None)?;
    Ok(memory.id)
}

fn apply_remote_okf(
    conn: &Connection,
    config: &Config,
    project: &str,
    remote: &GatewayMemory,
    target: Option<&str>,
) -> Result<String, MemoryError> {
    let (parsed, scope) = normalize_remote_okf(project, remote, target)?;

    // Complete model work before the short handler transaction.
    let embedding = embedding::embed_text(&parsed.concept.body, &config.model_cache_dir)?;
    let result = agent_memory::okf::OkfDocumentHandler::new(conn, scope)
        .put_with_operation(target, &parsed, None, false, "gateway_pull")
        .map_err(okf_cli_error)?;
    let blob = embedding_to_blob(&embedding);
    conn.execute(
        "UPDATE memories SET embedding = ?1, embedding_model = ?2 WHERE id = ?3",
        rusqlite::params![blob, EMBEDDING_MODEL_NAME_DEFAULT, result.id],
    )?;
    Ok(result.id)
}

fn normalize_remote_okf(
    project: &str,
    remote: &GatewayMemory,
    target: Option<&str>,
) -> Result<
    (
        agent_memory::okf::ParsedDocument,
        agent_memory::okf::BundleScope,
    ),
    MemoryError,
> {
    let envelope = remote
        .okf
        .as_ref()
        .ok_or_else(|| MemoryError::Config("gateway OKF envelope missing".to_string()))?;
    if envelope.version != 1 || envelope.format != "okf-markdown" {
        return Err(MemoryError::Config(format!(
            "unsupported gateway OKF envelope version/format: {}/{}",
            envelope.version, envelope.format
        )));
    }
    let mut parsed = agent_memory::okf::parse_document(&envelope.document)
        .map_err(|error| MemoryError::Config(format!("gateway OKF document: {error}")))?;
    if parsed.concept.body != remote.content {
        return Err(MemoryError::Config(
            "gateway legacy content and OKF body disagree".to_string(),
        ));
    }
    let scope = if project == GLOBAL_PROJECT_IDENT {
        agent_memory::okf::BundleScope::Global
    } else {
        agent_memory::okf::BundleScope::Project(project.to_string())
    };
    let metadata = parsed
        .concept
        .agent_memory
        .get_or_insert_with(Default::default);
    metadata.project = Some(project.to_string());
    metadata.scope = Some(if project == GLOBAL_PROJECT_IDENT {
        "global".to_string()
    } else {
        "project".to_string()
    });
    metadata.memory_type = Some(remote.memory_type.clone());
    metadata.revision = None;
    if let Some(target) = target {
        metadata.id = Some(target.to_string());
    }
    if !envelope.extensions.is_empty() {
        let yaml = serde_yaml_ng::to_value(&envelope.extensions)
            .map_err(|error| MemoryError::Config(error.to_string()))?;
        parsed
            .concept
            .extensions
            .insert("x-gateway-envelope".to_string(), yaml);
    }
    Ok((parsed, scope))
}

fn upsert_pull_sync(
    conn: &Connection,
    project: &str,
    local_memory_id: &str,
    remote: &GatewayMemory,
    tombstone_deleted: bool,
    tombstone: Option<&GatewayMemoryTombstone>,
) -> Result<(), MemoryError> {
    let gateway_memory_id = remote.gateway_memory_id.as_deref().ok_or_else(|| {
        MemoryError::Config("gateway pull memory missing gateway_memory_id".to_string())
    })?;
    let server_revision = remote.server_revision.ok_or_else(|| {
        MemoryError::Config("gateway pull memory missing server_revision".to_string())
    })?;
    queries::upsert_memory_gateway_sync(
        conn,
        &MemoryGatewaySyncUpsert {
            local_memory_id: local_memory_id.to_string(),
            project: project.to_string(),
            gateway_memory_id: gateway_memory_id.to_string(),
            last_seen_server_revision: server_revision,
            last_pushed_content_hash: None,
            last_pulled_content_hash: Some(remote.content_hash.clone()),
            sync_state: if tombstone_deleted {
                "tombstone".to_string()
            } else {
                "pulled".to_string()
            },
            tombstone_deleted,
            tombstone_at: tombstone.and_then(|t| t.deleted_at.clone()),
        },
    )?;
    Ok(())
}

fn reembed_memory(
    conn: &Connection,
    config: &Config,
    local_memory_id: &str,
    content: &str,
) -> Result<(), MemoryError> {
    let embedding = embedding::embed_text(content, &config.model_cache_dir)?;
    let blob = embedding_to_blob(&embedding);
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE memories SET embedding = ?1, embedding_model = ?2, updated_at = ?3
         WHERE id = ?4",
        rusqlite::params![blob, EMBEDDING_MODEL_NAME_DEFAULT, now, local_memory_id],
    )?;
    Ok(())
}

fn local_memory_hash(memory: &Memory) -> String {
    let tags = memory.tags.clone().unwrap_or_default();
    let memory_type = memory
        .memory_type
        .clone()
        .unwrap_or_else(|| "user".to_string());
    memory_content_hash(&memory.content, &memory_type, &tags)
}

fn print_pull_status(project: &str, plans: &[PullPlan], status_only: bool) {
    let conflicts = plans
        .iter()
        .filter(|plan| plan.action == "conflict")
        .count();
    println!(
        "{}",
        render::render_action_result(
            if status_only {
                "pull_status"
            } else {
                "pull_plan"
            },
            &[
                ("project", project.to_string()),
                ("remote", plans.len().to_string()),
                ("conflicts", conflicts.to_string()),
            ],
        )
    );
    for plan in plans {
        let mut attrs = vec![
            ("action", plan.action.to_string()),
            ("hash", plan.remote.content_hash.clone()),
            ("type", plan.remote.memory_type.clone()),
        ];
        if let Some(local_id) = plan.local_memory_id.as_deref() {
            attrs.push(("id", render::short_id(local_id).to_string()));
        }
        if let Some(local_hash) = plan.local_content_hash.as_deref() {
            attrs.push(("local_hash", local_hash.to_string()));
        }
        if let Some(gateway_id) = plan.remote.gateway_memory_id.as_deref() {
            attrs.push(("gateway_id", gateway_id.to_string()));
        }
        if let Some(rev) = plan.remote.server_revision {
            attrs.push(("server_revision", rev.to_string()));
        }
        println!("{}", render::render_action_result("pull_candidate", &attrs));
    }
}

fn resolve_gateway_project(cwd_project: Option<&str>) -> Result<String, MemoryError> {
    let project = cwd_project.ok_or_else(|| {
        MemoryError::Config("memory gateway exchange requires a current project ident".to_string())
    })?;
    if project == GLOBAL_PROJECT_IDENT {
        return Err(MemoryError::Config(
            "Global memories are excluded from gateway exchange".to_string(),
        ));
    }
    Ok(project.to_string())
}

fn map_gateway_error(err: GatewaySyncClientError) -> MemoryError {
    match err {
        GatewaySyncClientError::Config(msg) => MemoryError::Config(msg),
        other => MemoryError::Update(other.to_string()),
    }
}

/// Dispatch the `memory setup` family. Returns `anyhow::Result` so the
/// installer subcommands can use `anyhow::Context` freely; the caller wraps
/// the error into `MemoryError::Config` for the unified CLI exit path.
fn execute_setup(command: Option<SetupCommands>) -> anyhow::Result<()> {
    match command {
        None => menu::run_interactive(),
        Some(SetupCommands::Rules {
            target,
            all,
            dry_run,
            print,
            remove,
        }) => rules::run(target, all, dry_run, print, remove),
        Some(SetupCommands::Skill {
            dry_run,
            print,
            remove,
        }) => skill::run(dry_run, print, remove),
        Some(SetupCommands::Hooks {
            dry_run,
            print,
            remove,
        }) => hooks::run(true, dry_run, print, remove),
        Some(SetupCommands::All { yes }) => menu::run_all(yes),
        Some(SetupCommands::Gateway) => gateway::run(),
    }
}

/// Resolved boost configuration for one `search`/`context` call.
///
/// The two boosts are independent: disabling the current-project boost via
/// `--no-project-boost` leaves the global boost intact so universal
/// preferences continue to surface across every repo even when local
/// boosting is off. Global scope is always on unless a future
/// `--no-global-boost` flag is added; the sentinel identifier is hardcoded
/// so the retrieval behavior is deterministic.
pub(crate) struct BoostConfig<'a> {
    pub current_project: Option<&'a str>,
    pub global_project: Option<&'a str>,
    pub project_boost: f32,
    pub global_boost: f32,
}

fn resolve_boosts<'a>(
    explicit: Option<&'a str>,
    cwd: Option<&'a str>,
    disable: bool,
) -> BoostConfig<'a> {
    if disable {
        // --no-project-boost disables BOTH boosts so `memory search --no-project-boost`
        // gives a genuinely flat ranking. Users who want "flat current, but
        // still boost global" are a sufficiently niche case that we wait
        // for them to ask before plumbing a third flag.
        BoostConfig {
            current_project: None,
            global_project: None,
            project_boost: 1.0,
            global_boost: 1.0,
        }
    } else {
        BoostConfig {
            current_project: explicit.or(cwd),
            global_project: Some(GLOBAL_PROJECT_IDENT),
            project_boost: PROJECT_BOOST,
            global_boost: GLOBAL_BOOST,
        }
    }
}

/// Assemble the reflection `hint` string for `context`/`search` responses.
///
/// Four independent conditions, each additive:
///
/// 1. **Cross-project results present** — original behavior: prefix the hint
///    with the cross-project ratio so the agent knows to treat them as
///    prior-art rather than direct context.
/// 2. **Zero global-scope matches in top-K** — reflection prompt nudging the
///    agent to confirm no universal user preference applies before acting.
///    Emitted whenever `current_project` is set (i.e. we're doing a scoped
///    retrieval, not a flat query), even if everything else lines up —
///    silence here is the dangerous case, not noise.
/// 3. **Global-scope matches present** — surface the count so the agent
///    treats them as directives rather than suggestions.
/// 4. **Canonical-state intent in the query** — the task mentions git, a
///    version, a tag, a release, etc. Memory does not store git history,
///    so nudge the agent to consult git/release tooling for those facts
///    and to use memory for *how/why/what* knowledge instead.
///
/// Returns `None` when nothing useful would be said (no current-project
/// context AND no global hits AND no cross-project hits AND no canonical
/// intent).
fn results_hint(
    cross: usize,
    globals: usize,
    total: usize,
    current: Option<&str>,
    query: &str,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(cp) = current {
        if cross > 0 {
            parts.push(if cross == total && globals == 0 {
                format!(
                    "All {total} results are from other projects (no memories tagged '{cp}' matched). Treat as prior-art or general guidance, not direct context for the current project."
                )
            } else {
                format!(
                    "{cross} of {total} results are cross-project (is_current_project=false). Use those as prior-art or general guidance, not direct context for '{cp}'. Use `memory get <id>` for full content."
                )
            });
        }
    }

    if globals > 0 {
        parts.push(format!(
            "{globals} of {total} results are global-scope preferences (apply across all projects). Treat them as directives, not suggestions."
        ));
    } else if current.is_some() {
        // Reflection prompt fires only when we would otherwise have surfaced
        // globals (i.e. during a scoped retrieval). In flat `--no-project-boost`
        // mode the global boost is off too, so the absence isn't meaningful.
        parts.push(
            "No global-scope preferences matched this task. If the user has stated a general rule relevant to this domain, it did not surface — consider asking before acting if you suspect one exists."
                .to_string(),
        );
    }

    if let Some(h) = canonical_state_hint(query) {
        parts.push(h.to_string());
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Return the canonical-state nudge when `text` reads like a git/release/
/// version lookup, otherwise `None`. Used by both the search/context hint
/// path and the store path so an agent both *seeking* and *saving* such
/// facts gets the same reminder.
fn canonical_state_hint(text: &str) -> Option<&'static str> {
    if has_canonical_state_intent(text) {
        Some(
            "This looks like a git/version/release lookup. Memory does not store git history, version numbers, commit SHAs, or release tags — use `git log`, `git tag`, `git show`, or your release tooling for those facts. Memory is for the *how/why/what* a cold agent needs to ramp up: project layout, workflows, decisions, gotchas, and reusable procedures.",
        )
    } else {
        None
    }
}

/// Cheap word-level scan for canonical-state vocabulary. Splits on
/// non-alphanumeric and checks tokens against a curated keyword set so
/// `committing`, `tagged`, `versions`, etc. all match. The keyword list is
/// deliberately broad — false positives only print an informational hint;
/// false negatives let bad memories slip through.
fn has_canonical_state_intent(text: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "git",
        "commit",
        "commits",
        "committed",
        "committing",
        "tag",
        "tags",
        "tagged",
        "tagging",
        "version",
        "versions",
        "versioned",
        "versioning",
        "semver",
        "release",
        "releases",
        "released",
        "releasing",
        "changelog",
        "push",
        "pushed",
        "pushing",
        "rebase",
        "merge",
        "branch",
        "sha",
    ];
    for token in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        // Lowercase comparison without allocating per token: tokens are short.
        if KEYWORDS
            .iter()
            .any(|k| k.len() == token.len() && k.eq_ignore_ascii_case(token))
        {
            return true;
        }
    }
    false
}

fn scope_label(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Project => "project",
        MemoryScope::Global => "global",
    }
}

fn store_scope_hint() -> String {
    "Stored as project-scoped. If this preference applies across all projects, re-run with `--scope global` — a silent mis-classification means future sessions in other projects won't see it.".to_string()
}

fn store_quality_hint() -> &'static str {
    "Before relying on this memory, verify it passes the quality gate: it should preserve reusable guidance, a preference, procedure, non-obvious constraint, failure cause, or a pointer to the canonical system specified by user/repo/tool guidance plus why it matters. Prefer updating an existing related memory over creating a new one; if this only records state visible in git, repo files, CI, releases, tasks, comms, notes, or configured pattern systems, forget or rewrite it."
}

/// Treat empty strings as "no project" for the move/copy `--from`/`--to` flags.
/// Lets users explicitly target or assign a NULL project without a second flag.
fn empty_to_none(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn resolve_working_project(
    explicit: Option<String>,
    cwd_project: Option<&str>,
) -> Result<String, MemoryError> {
    let project = explicit
        .or_else(|| cwd_project.map(str::to_string))
        .ok_or_else(|| {
            MemoryError::Config(
                "WorkingContext requires a project ident; run from a project or pass --project"
                    .to_string(),
            )
        })?;

    if project.trim().is_empty() {
        return Err(MemoryError::Config(
            "WorkingContext requires a non-empty project ident".to_string(),
        ));
    }
    if project == GLOBAL_PROJECT_IDENT {
        return Err(MemoryError::Config(format!(
            "`{GLOBAL_PROJECT_IDENT}` is reserved for global-scoped memories; \
             WorkingContext is per-project only"
        )));
    }

    Ok(project)
}

fn read_working_content(
    content: Option<String>,
    file: Option<PathBuf>,
) -> Result<String, MemoryError> {
    match (content, file) {
        (Some(_), Some(_)) => Err(MemoryError::Config(
            "`memory working set` accepts either inline content or --file, not both".to_string(),
        )),
        (Some(raw), None) if raw == "-" => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
        (Some(raw), None) => Ok(raw),
        (None, Some(path)) => Ok(std::fs::read_to_string(path)?),
        (None, None) => Err(MemoryError::Config(
            "`memory working set` requires content, '-' for stdin, or --file PATH".to_string(),
        )),
    }
}

fn working_context_move_preview(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<&'static str, MemoryError> {
    let Some(from) = from else {
        return Ok("none");
    };
    if from == GLOBAL_PROJECT_IDENT || queries::get_working_context(conn, from)?.is_none() {
        return Ok("none");
    }

    match to {
        Some(target) if target == from => Ok("none"),
        Some(target) if queries::get_working_context(conn, target)?.is_some() => {
            Ok("target_exists")
        }
        Some(_) => Ok("would_move"),
        None => Ok("would_delete"),
    }
}

fn move_working_context_for_project_move(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<queries::WorkingContextProjectMove, MemoryError> {
    match from {
        Some(project) if project != GLOBAL_PROJECT_IDENT => {
            queries::move_working_context_project(conn, project, to)
        }
        _ => Ok(queries::WorkingContextProjectMove::None),
    }
}

/// Execute a `memory move` and print a light-XML `<result>` line.
///
/// `id` may be `None` (use `from`), `Some(full_uuid)`, or `Some("")` — the
/// empty string is a sentinel from [`resolve_id_arg`] meaning "the user
/// supplied an ID but it was ambiguous or not found; the disambiguation
/// prompt has already been printed, do nothing here".
fn run_move(
    conn: &Connection,
    id: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    dry_run: bool,
) -> Result<(), MemoryError> {
    match (id, from) {
        (Some(""), _) => Ok(()), // unresolved sentinel — already handled
        (Some(id), _) => {
            if dry_run {
                let mem = queries::get_memory_by_id(conn, id)?;
                let mut attrs: Vec<(&str, String)> = vec![
                    ("id", render::short_id(&mem.id).to_string()),
                    ("would_move", "1".to_string()),
                ];
                if let Some(p) = mem.project.as_deref() {
                    attrs.push(("from_project", p.to_string()));
                }
                if let Some(t) = to {
                    attrs.push(("to_project", t.to_string()));
                }
                println!("{}", render::render_action_result("dry_run", &attrs));
            } else {
                let changed = queries::move_memory_by_id(conn, id, to)?;
                let status = if changed { "moved" } else { "not_found" };
                let mut attrs: Vec<(&str, String)> = vec![("id", render::short_id(id).to_string())];
                if let Some(t) = to {
                    attrs.push(("to_project", t.to_string()));
                }
                println!("{}", render::render_action_result(status, &attrs));
            }
            Ok(())
        }
        (None, Some(from)) => {
            let from_opt = empty_to_none(from);
            if dry_run {
                let mems = queries::list_memories_by_project(conn, from_opt)?;
                let working_context = working_context_move_preview(conn, from_opt, to)?;
                let mut attrs: Vec<(&str, String)> = vec![
                    ("would_move", mems.len().to_string()),
                    ("from_project", from_opt.unwrap_or("").to_string()),
                    ("working_context", working_context.to_string()),
                ];
                if let Some(t) = to {
                    attrs.push(("to_project", t.to_string()));
                }
                println!("{}", render::render_action_result("dry_run", &attrs));
            } else {
                let working_context = move_working_context_for_project_move(conn, from_opt, to)?;
                let count = queries::move_memories_by_project(conn, from_opt, to)?;
                let mut attrs: Vec<(&str, String)> = vec![
                    ("count", count.to_string()),
                    ("from_project", from_opt.unwrap_or("").to_string()),
                    ("working_context", working_context.as_str().to_string()),
                ];
                if let Some(t) = to {
                    attrs.push(("to_project", t.to_string()));
                }
                println!("{}", render::render_action_result("moved", &attrs));
            }
            Ok(())
        }
        (None, None) => {
            println!(
                "{}",
                render::render_action_result(
                    "error",
                    &[(
                        "message",
                        "Either --id or --from must be provided".to_string()
                    )]
                )
            );
            Ok(())
        }
    }
}

/// Execute a `memory copy` and print a light-XML `<result>` line. See
/// [`run_move`] for the sentinel-handling contract on `id`.
fn run_copy(
    conn: &Connection,
    id: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    dry_run: bool,
) -> Result<(), MemoryError> {
    match (id, from) {
        (Some(""), _) => Ok(()), // unresolved sentinel — already handled
        (Some(id), _) => {
            if dry_run {
                let mem = queries::get_memory_by_id(conn, id)?;
                let mut attrs: Vec<(&str, String)> = vec![
                    ("source_id", render::short_id(&mem.id).to_string()),
                    ("would_copy", "1".to_string()),
                ];
                if let Some(p) = mem.project.as_deref() {
                    attrs.push(("from_project", p.to_string()));
                }
                if let Some(t) = to {
                    attrs.push(("to_project", t.to_string()));
                }
                println!("{}", render::render_action_result("dry_run", &attrs));
            } else {
                let new_id = queries::copy_memory_by_id(conn, id, to)?;
                let mut attrs: Vec<(&str, String)> = vec![
                    ("source_id", render::short_id(id).to_string()),
                    ("new_id", render::short_id(&new_id).to_string()),
                ];
                if let Some(t) = to {
                    attrs.push(("to_project", t.to_string()));
                }
                println!("{}", render::render_action_result("copied", &attrs));
            }
            Ok(())
        }
        (None, Some(from)) => {
            let from_opt = empty_to_none(from);
            if dry_run {
                let mems = queries::list_memories_by_project(conn, from_opt)?;
                let mut attrs: Vec<(&str, String)> = vec![
                    ("would_copy", mems.len().to_string()),
                    ("from_project", from_opt.unwrap_or("").to_string()),
                ];
                if let Some(t) = to {
                    attrs.push(("to_project", t.to_string()));
                }
                println!("{}", render::render_action_result("dry_run", &attrs));
            } else {
                let new_ids = queries::copy_memories_by_project(conn, from_opt, to)?;
                let mut attrs: Vec<(&str, String)> = vec![
                    ("count", new_ids.len().to_string()),
                    ("from_project", from_opt.unwrap_or("").to_string()),
                ];
                if let Some(t) = to {
                    attrs.push(("to_project", t.to_string()));
                }
                println!("{}", render::render_action_result("copied", &attrs));
            }
            Ok(())
        }
        (None, None) => {
            println!(
                "{}",
                render::render_action_result(
                    "error",
                    &[(
                        "message",
                        "Either --id or --from must be provided".to_string()
                    )]
                )
            );
            Ok(())
        }
    }
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::PushMemoryResult;
    use clap::Parser;
    use serde_json::{json, Value};

    /// `memory hook --agent codex` parses to `Cli::Hook` with the chosen agent
    /// and the default limit of 5.
    #[test]
    fn parse_hook_agent_and_default_limit() {
        let cli = Cli::try_parse_from(["memory", "hook", "--agent", "codex"]).unwrap();
        match cli {
            Cli::Hook { agent, limit } => {
                assert_eq!(agent, "codex");
                assert_eq!(limit, 5);
            }
            _ => panic!("expected Hook variant"),
        }
    }

    /// `memory hook` with no `--agent` defaults to "claude".
    #[test]
    fn parse_hook_defaults_to_claude() {
        let cli = Cli::try_parse_from(["memory", "hook"]).unwrap();
        match cli {
            Cli::Hook { agent, limit } => {
                assert_eq!(agent, "claude");
                assert_eq!(limit, 5);
            }
            _ => panic!("expected Hook variant"),
        }
    }

    /// `--scope global` should parse to `MemoryScope::Global` and leave other
    /// flags untouched.
    #[test]
    fn parse_store_scope_global() {
        let cli = Cli::try_parse_from(["memory", "store", "hello", "--scope", "global"]).unwrap();
        match cli {
            Cli::Store { scope, content, .. } => {
                assert_eq!(scope, Some(MemoryScope::Global));
                assert_eq!(content, "hello");
            }
            _ => panic!("expected Store variant"),
        }
    }

    /// Omitting `--scope` must leave the field `None` (not a default) so the
    /// dispatch layer can distinguish "not chosen" from "explicitly project".
    #[test]
    fn parse_store_scope_absent_is_none() {
        let cli = Cli::try_parse_from(["memory", "store", "hello"]).unwrap();
        match cli {
            Cli::Store { scope, .. } => assert_eq!(scope, None),
            _ => panic!("expected Store variant"),
        }
    }

    /// Explicit `--scope project` is distinguishable from the default and
    /// suppresses the reflection hint.
    #[test]
    fn parse_store_scope_project_explicit() {
        let cli = Cli::try_parse_from(["memory", "store", "hi", "--scope", "project"]).unwrap();
        match cli {
            Cli::Store { scope, .. } => assert_eq!(scope, Some(MemoryScope::Project)),
            _ => panic!("expected Store variant"),
        }
    }

    /// Bare `memory update` still parses as the self-updater (no id, no
    /// content). The dispatch layer routes to the manual-update path.
    #[test]
    fn parse_update_bare_is_self_updater() {
        let cli = Cli::try_parse_from(["memory", "update"]).unwrap();
        match cli {
            Cli::Update {
                id,
                content,
                tags,
                memory_type,
                ..
            } => {
                assert!(id.is_none());
                assert!(content.is_none());
                assert!(tags.is_none());
                assert!(memory_type.is_none());
            }
            _ => panic!("expected Update variant"),
        }
    }

    /// `memory update <id> --content "..."` parses the content-update form.
    #[test]
    fn parse_update_content_form() {
        let cli = Cli::try_parse_from([
            "memory",
            "update",
            "aabbccdd",
            "--content",
            "new body\n- fact",
            "--tags",
            "a,b",
            "-m",
            "project",
        ])
        .unwrap();
        match cli {
            Cli::Update {
                id,
                content,
                tags,
                memory_type,
                ..
            } => {
                assert_eq!(id.as_deref(), Some("aabbccdd"));
                assert_eq!(content.as_deref(), Some("new body\n- fact"));
                assert_eq!(tags.as_deref(), Some("a,b"));
                assert_eq!(memory_type.as_deref(), Some("project"));
            }
            _ => panic!("expected Update variant"),
        }
    }

    #[test]
    fn parse_push_and_pull_status_commands() {
        let push = Cli::try_parse_from(["memory", "push", "status"]).unwrap();
        match push {
            Cli::Push {
                all,
                command,
                batch_size,
            } => {
                assert!(!all);
                assert!(matches!(command, Some(GatewayTransferCommand::Status)));
                assert_eq!(batch_size, DEFAULT_GATEWAY_PUSH_BATCH_SIZE);
            }
            _ => panic!("expected Push variant"),
        }

        let push_run = Cli::try_parse_from(["memory", "push"]).unwrap();
        match push_run {
            Cli::Push {
                all,
                command,
                batch_size,
            } => {
                assert!(!all);
                assert!(command.is_none());
                assert_eq!(batch_size, DEFAULT_GATEWAY_PUSH_BATCH_SIZE);
            }
            _ => panic!("expected Push variant"),
        }

        let push_batch_size =
            Cli::try_parse_from(["memory", "push", "--batch-size", "123"]).unwrap();
        match push_batch_size {
            Cli::Push {
                all,
                command,
                batch_size,
            } => {
                assert!(!all);
                assert!(command.is_none());
                assert_eq!(batch_size, 123);
            }
            _ => panic!("expected Push variant"),
        }

        let push_all =
            Cli::try_parse_from(["memory", "push", "--all", "--batch-size", "123"]).unwrap();
        match push_all {
            Cli::Push {
                all,
                command,
                batch_size,
            } => {
                assert!(all);
                assert!(command.is_none());
                assert_eq!(batch_size, 123);
            }
            _ => panic!("expected Push variant"),
        }

        let push_all_status = Cli::try_parse_from(["memory", "push", "--all", "status"]).unwrap();
        match push_all_status {
            Cli::Push {
                all,
                command,
                batch_size,
            } => {
                assert!(all);
                assert!(matches!(command, Some(GatewayTransferCommand::Status)));
                assert_eq!(batch_size, DEFAULT_GATEWAY_PUSH_BATCH_SIZE);
            }
            _ => panic!("expected Push variant"),
        }

        let pull = Cli::try_parse_from(["memory", "pull", "status"]).unwrap();
        match pull {
            Cli::Pull { all, command } => {
                assert!(!all);
                assert!(matches!(command, Some(GatewayTransferCommand::Status)));
            }
            _ => panic!("expected Pull variant"),
        }

        let pull_all = Cli::try_parse_from(["memory", "pull", "--all"]).unwrap();
        match pull_all {
            Cli::Pull { all, command } => {
                assert!(all);
                assert!(command.is_none());
            }
            _ => panic!("expected Pull variant"),
        }

        let pull_all_status = Cli::try_parse_from(["memory", "pull", "--all", "status"]).unwrap();
        match pull_all_status {
            Cli::Pull { all, command } => {
                assert!(all);
                assert!(matches!(command, Some(GatewayTransferCommand::Status)));
            }
            _ => panic!("expected Pull variant"),
        }
    }

    #[test]
    fn parse_setup_gateway_command() {
        let cli = Cli::try_parse_from(["memory", "setup", "gateway"]).unwrap();
        match cli {
            Cli::Setup { command } => assert!(matches!(command, Some(SetupCommands::Gateway))),
            _ => panic!("expected Setup variant"),
        }
    }

    #[test]
    fn parse_setup_hooks_command() {
        let cli = Cli::try_parse_from(["memory", "setup", "hooks"]).unwrap();
        match cli {
            Cli::Setup { command } => assert!(matches!(
                command,
                Some(SetupCommands::Hooks {
                    dry_run: false,
                    print: false,
                    remove: false,
                })
            )),
            _ => panic!("expected Setup variant"),
        }
    }

    #[test]
    fn parse_setup_hooks_command_with_flags() {
        let cli =
            Cli::try_parse_from(["memory", "setup", "hooks", "--dry-run", "--remove"]).unwrap();
        match cli {
            Cli::Setup { command } => assert!(matches!(
                command,
                Some(SetupCommands::Hooks {
                    dry_run: true,
                    print: false,
                    remove: true,
                })
            )),
            _ => panic!("expected Setup variant"),
        }
    }

    #[test]
    fn push_candidates_track_create_and_skipped_without_metadata_writes() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");
        let mut memory = Memory::new(
            "project memory".to_string(),
            Some(vec!["gateway".to_string()]),
            Some("agent-memory".to_string()),
            None,
            None,
            Some("project".to_string()),
        );
        memory.id = "aaaaaaaa-0000-1111-2222-000000000001".to_string();
        queries::insert_memory(&conn, &memory).unwrap();

        let first = build_push_candidates(&conn, "agent-memory").unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].action, "create");
        let hash = first[0].gateway_memory.content_hash.clone();

        queries::upsert_memory_gateway_sync(
            &conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: memory.id.clone(),
                project: "agent-memory".to_string(),
                gateway_memory_id: "gw-1".to_string(),
                last_seen_server_revision: 3,
                last_pushed_content_hash: Some(hash.clone()),
                last_pulled_content_hash: None,
                sync_state: "created".to_string(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )
        .unwrap();

        let second = build_push_candidates(&conn, "agent-memory").unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].action, "skipped");
        assert_eq!(
            second[0].gateway_memory.base_server_revision,
            Some(3),
            "linked records carry base revision"
        );
        assert_eq!(second[0].gateway_memory.content_hash, hash);
    }

    #[test]
    fn push_candidates_are_project_scoped_and_allow_global_scope() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");

        for (id, content, project) in [
            (
                "aaaaaaaa-0000-1111-2222-000000000010",
                "current project memory",
                "agent-memory",
            ),
            (
                "aaaaaaaa-0000-1111-2222-000000000011",
                "other project memory",
                "other-project",
            ),
            (
                "aaaaaaaa-0000-1111-2222-000000000012",
                "global memory",
                GLOBAL_PROJECT_IDENT,
            ),
        ] {
            let mut memory = Memory::new(
                content.to_string(),
                Some(vec!["gateway".to_string()]),
                Some(project.to_string()),
                None,
                None,
                Some("project".to_string()),
            );
            memory.id = id.to_string();
            queries::insert_memory(&conn, &memory).unwrap();
        }

        let candidates = build_push_candidates(&conn, "agent-memory").unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].local_memory_id,
            "aaaaaaaa-0000-1111-2222-000000000010"
        );

        let global = build_push_candidates(&conn, GLOBAL_PROJECT_IDENT).unwrap();
        assert_eq!(global.len(), 1);
        assert_eq!(
            global[0].local_memory_id,
            "aaaaaaaa-0000-1111-2222-000000000012"
        );
        assert_eq!(global[0].gateway_memory.project, GLOBAL_PROJECT_IDENT);
        assert_eq!(
            global[0]
                .gateway_memory
                .provenance
                .as_ref()
                .and_then(|p| p.source_os.as_deref()),
            Some(env::consts::OS)
        );
    }

    #[test]
    fn push_candidates_skip_unsynced_exact_duplicate_of_synced_record() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");
        let first = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000013",
            "same project memory",
            vec!["gateway"],
            "agent-memory",
        );
        let second = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000014",
            "same project memory",
            vec!["gateway"],
            "agent-memory",
        );
        let hash = local_memory_hash(&first);

        queries::upsert_memory_gateway_sync(
            &conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: first.id.clone(),
                project: "agent-memory".to_string(),
                gateway_memory_id: "gw-duplicate".to_string(),
                last_seen_server_revision: 9,
                last_pushed_content_hash: Some(hash.clone()),
                last_pulled_content_hash: None,
                sync_state: "created".to_string(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )
        .unwrap();

        let candidates = build_push_candidates(&conn, "agent-memory").unwrap();
        let first_candidate = candidates
            .iter()
            .find(|candidate| candidate.local_memory_id == first.id)
            .unwrap();
        let second_candidate = candidates
            .iter()
            .find(|candidate| candidate.local_memory_id == second.id)
            .unwrap();

        assert_eq!(first_candidate.action, "skipped");
        assert_eq!(second_candidate.action, "skipped");
        assert_eq!(
            second_candidate.gateway_memory.gateway_memory_id.as_deref(),
            Some("gw-duplicate")
        );
        assert_eq!(
            second_candidate.gateway_memory.base_server_revision,
            Some(9)
        );
        assert_eq!(second_candidate.gateway_memory.content_hash, hash);
    }

    #[test]
    fn local_gateway_projects_include_global_and_exclude_unscoped() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");

        for (id, project) in [
            ("aaaaaaaa-0000-1111-2222-000000000020", Some("agent-memory")),
            (
                "aaaaaaaa-0000-1111-2222-000000000021",
                Some(GLOBAL_PROJECT_IDENT),
            ),
            ("aaaaaaaa-0000-1111-2222-000000000022", None),
        ] {
            let mut memory = Memory::new(
                format!("memory {id}"),
                Some(vec!["gateway".to_string()]),
                project.map(str::to_string),
                None,
                None,
                Some("project".to_string()),
            );
            memory.id = id.to_string();
            queries::insert_memory(&conn, &memory).unwrap();
        }

        let projects = list_local_gateway_projects(&conn).unwrap();
        assert_eq!(
            projects,
            vec![GLOBAL_PROJECT_IDENT.to_string(), "agent-memory".to_string()]
        );
    }

    #[test]
    fn local_gateway_projects_include_pending_delete_without_live_memory() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");
        let memory = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000023",
            "delete-only project memory",
            vec!["gateway"],
            "delete-only",
        );
        link_cli_test_memory(&conn, &memory, "gw-delete-only", 17);

        queries::delete_memory(&conn, &memory.id).unwrap();

        let projects = list_local_gateway_projects(&conn).unwrap();
        assert_eq!(projects, vec!["delete-only".to_string()]);
    }

    #[test]
    fn push_candidates_include_pending_gateway_delete_after_local_delete() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");
        let memory = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000024",
            "locally forgotten memory",
            vec!["gateway"],
            "agent-memory",
        );
        let hash = local_memory_hash(&memory);
        link_cli_test_memory(&conn, &memory, "gw-forgotten", 23);

        queries::delete_memory(&conn, &memory.id).unwrap();

        let candidates = build_push_candidates(&conn, "agent-memory").unwrap();
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.local_memory_id, memory.id);
        assert_eq!(candidate.action, "delete");
        assert_eq!(
            candidate.gateway_memory.gateway_memory_id.as_deref(),
            Some("gw-forgotten")
        );
        assert_eq!(candidate.gateway_memory.base_server_revision, Some(23));
        assert_eq!(candidate.gateway_memory.content_hash, hash);
        assert_eq!(candidate.gateway_memory.content, "");
        assert_eq!(
            candidate
                .gateway_memory
                .tombstone
                .as_ref()
                .map(|tombstone| tombstone.deleted),
            Some(true)
        );
    }

    #[test]
    fn push_batch_size_validation_respects_gateway_cap() {
        assert_eq!(validate_push_batch_size(1).unwrap(), 1);
        assert_eq!(
            validate_push_batch_size(MAX_GATEWAY_PUSH_BATCH_SIZE).unwrap(),
            MAX_GATEWAY_PUSH_BATCH_SIZE
        );
        assert!(validate_push_batch_size(0)
            .unwrap_err()
            .to_string()
            .contains("at least 1"));
        assert!(validate_push_batch_size(MAX_GATEWAY_PUSH_BATCH_SIZE + 1)
            .unwrap_err()
            .to_string()
            .contains("no greater than 500"));
    }

    #[test]
    fn push_batches_default_to_unlimited_total_with_capped_requests() {
        let candidates: Vec<PushCandidate> = (0..1001).map(push_test_candidate).collect();
        let pending: Vec<&PushCandidate> = candidates.iter().collect();

        let batches = push_batches(&pending, DEFAULT_GATEWAY_PUSH_BATCH_SIZE);

        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 450);
        assert_eq!(batches[1].len(), 450);
        assert_eq!(batches[2].len(), 101);
        assert!(batches
            .iter()
            .all(|batch| batch.len() <= MAX_GATEWAY_PUSH_BATCH_SIZE));

        let (request, hashes) = build_push_request("agent-memory", batches[0]);
        assert_eq!(request.memories.len(), DEFAULT_GATEWAY_PUSH_BATCH_SIZE);
        assert_eq!(hashes.len(), DEFAULT_GATEWAY_PUSH_BATCH_SIZE);
        request.validate_project_scope().unwrap();
    }

    #[test]
    fn push_batch_apply_makes_rerun_skip_successful_records() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");
        let first = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000101",
            "first pending push memory",
            vec!["gateway"],
            "agent-memory",
        );
        let second = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000102",
            "second pending push memory",
            vec!["gateway"],
            "agent-memory",
        );
        let third = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000103",
            "third pending push memory",
            vec!["gateway"],
            "agent-memory",
        );

        let candidates = build_push_candidates(&conn, "agent-memory").unwrap();
        let pending: Vec<&PushCandidate> = candidates
            .iter()
            .filter(|candidate| candidate.action != "skipped")
            .collect();
        let batches = push_batches(&pending, 2);
        assert_eq!(batches.len(), 2);

        let (_request, hashes) = build_push_request("agent-memory", batches[0]);
        let response = PushMemoriesResponse {
            project: "agent-memory".to_string(),
            server_revision: Some(2),
            results: batches[0]
                .iter()
                .enumerate()
                .map(|(index, candidate)| PushMemoryResult {
                    local_memory_id: Some(candidate.local_memory_id.clone()),
                    client_id: None,
                    gateway_memory_id: Some(format!("gw-{index}")),
                    server_revision: Some((index + 1) as i64),
                    action: PushMemoryAction::Created,
                    content_hash: Some(candidate.gateway_memory.content_hash.clone()),
                    conflict: None,
                    error: None,
                    errors: Vec::new(),
                })
                .collect(),
        };

        apply_push_response(&conn, "agent-memory", &response, &hashes).unwrap();

        let resumed = build_push_candidates(&conn, "agent-memory").unwrap();
        let first_action = resumed
            .iter()
            .find(|candidate| candidate.local_memory_id == first.id)
            .unwrap()
            .action;
        let second_action = resumed
            .iter()
            .find(|candidate| candidate.local_memory_id == second.id)
            .unwrap()
            .action;
        let remaining_pending: Vec<&PushCandidate> = resumed
            .iter()
            .filter(|candidate| candidate.action != "skipped")
            .collect();

        assert_eq!(first_action, "skipped");
        assert_eq!(second_action, "skipped");
        assert_eq!(remaining_pending.len(), 1);
        assert_eq!(remaining_pending[0].local_memory_id, third.id);
        assert_eq!(remaining_pending[0].action, "create");
    }

    #[test]
    fn push_apply_ignores_linked_duplicate_already_mapped_to_same_gateway_id() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");
        let first = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000104",
            "same pushed memory",
            vec!["gateway"],
            "agent-memory",
        );
        let second = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000105",
            "same pushed memory",
            vec!["gateway"],
            "agent-memory",
        );
        let hash = local_memory_hash(&first);

        queries::upsert_memory_gateway_sync(
            &conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: first.id.clone(),
                project: "agent-memory".to_string(),
                gateway_memory_id: "gw-duplicate".to_string(),
                last_seen_server_revision: 9,
                last_pushed_content_hash: Some(hash.clone()),
                last_pulled_content_hash: None,
                sync_state: "created".to_string(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )
        .unwrap();

        let response = PushMemoriesResponse {
            project: "agent-memory".to_string(),
            server_revision: Some(10),
            results: vec![PushMemoryResult {
                local_memory_id: Some(second.id.clone()),
                client_id: None,
                gateway_memory_id: Some("gw-duplicate".to_string()),
                server_revision: Some(10),
                action: PushMemoryAction::Linked,
                content_hash: Some(hash),
                conflict: None,
                error: None,
                errors: Vec::new(),
            }],
        };
        let hashes = HashMap::new();

        apply_push_response(&conn, "agent-memory", &response, &hashes).unwrap();

        let first_sync = queries::get_memory_gateway_sync(&conn, &first.id)
            .unwrap()
            .expect("first sync row");
        assert_eq!(first_sync.gateway_memory_id, "gw-duplicate");
        assert!(queries::get_memory_gateway_sync(&conn, &second.id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn push_apply_clears_pending_delete_after_gateway_ack() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");
        let tombstoned = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000106",
            "pending tombstone",
            vec!["gateway"],
            "agent-memory",
        );
        let updated = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000107",
            "pending update tombstone",
            vec!["gateway"],
            "agent-memory",
        );
        link_cli_test_memory(&conn, &tombstoned, "gw-tombstoned", 10);
        link_cli_test_memory(&conn, &updated, "gw-updated-tombstone", 11);
        queries::delete_memory(&conn, &tombstoned.id).unwrap();
        queries::delete_memory(&conn, &updated.id).unwrap();

        let response = PushMemoriesResponse {
            project: "agent-memory".to_string(),
            server_revision: Some(12),
            results: vec![
                PushMemoryResult {
                    local_memory_id: Some(tombstoned.id.clone()),
                    client_id: None,
                    gateway_memory_id: Some("gw-tombstoned".to_string()),
                    server_revision: Some(12),
                    action: PushMemoryAction::Tombstoned,
                    content_hash: None,
                    conflict: None,
                    error: None,
                    errors: Vec::new(),
                },
                PushMemoryResult {
                    local_memory_id: Some(updated.id.clone()),
                    client_id: None,
                    gateway_memory_id: Some("gw-updated-tombstone".to_string()),
                    server_revision: Some(13),
                    action: PushMemoryAction::Updated,
                    content_hash: None,
                    conflict: None,
                    error: None,
                    errors: Vec::new(),
                },
            ],
        };

        apply_push_response(&conn, "agent-memory", &response, &HashMap::new()).unwrap();

        assert!(queries::get_memory_gateway_sync(&conn, &tombstoned.id)
            .unwrap()
            .is_none());
        assert!(queries::get_memory_gateway_sync(&conn, &updated.id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn push_conflict_response_leaves_existing_sync_metadata_unchanged() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");
        let mut memory = Memory::new(
            "local project memory".to_string(),
            Some(vec!["gateway".to_string()]),
            Some("agent-memory".to_string()),
            None,
            None,
            Some("project".to_string()),
        );
        memory.id = "aaaaaaaa-0000-1111-2222-000000000020".to_string();
        queries::insert_memory(&conn, &memory).unwrap();

        queries::upsert_memory_gateway_sync(
            &conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: memory.id.clone(),
                project: "agent-memory".to_string(),
                gateway_memory_id: "gw-conflict".to_string(),
                last_seen_server_revision: 3,
                last_pushed_content_hash: Some("old-hash".to_string()),
                last_pulled_content_hash: Some("old-pulled-hash".to_string()),
                sync_state: "created".to_string(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )
        .unwrap();

        let response = PushMemoriesResponse {
            project: "agent-memory".to_string(),
            server_revision: Some(4),
            results: vec![PushMemoryResult {
                local_memory_id: Some(memory.id.clone()),
                client_id: None,
                gateway_memory_id: Some("gw-conflict".to_string()),
                server_revision: Some(4),
                action: PushMemoryAction::Conflict,
                content_hash: Some("new-hash".to_string()),
                conflict: None,
                error: None,
                errors: Vec::new(),
            }],
        };
        let hashes = HashMap::from([(memory.id.clone(), "new-hash".to_string())]);

        apply_push_response(&conn, "agent-memory", &response, &hashes).unwrap();

        let sync = queries::get_memory_gateway_sync(&conn, &memory.id)
            .unwrap()
            .expect("sync row");
        assert_eq!(sync.last_seen_server_revision, 3);
        assert_eq!(sync.last_pushed_content_hash.as_deref(), Some("old-hash"));
        assert_eq!(
            sync.last_pulled_content_hash.as_deref(),
            Some("old-pulled-hash")
        );
        assert_eq!(sync.sync_state, "created");
    }

    fn cli_test_config() -> Config {
        Config {
            data_dir: PathBuf::new(),
            db_path: PathBuf::new(),
            model_cache_dir: PathBuf::new(),
            gateway: Default::default(),
        }
    }

    fn cli_test_config_with_gateway(auto_sync: Option<bool>) -> Config {
        let mut config = cli_test_config();
        config.gateway.base_url = Some("http://127.0.0.1:1".to_string());
        config.gateway.api_key = Some("test-key".to_string());
        config.gateway.auto_sync = auto_sync;
        config
    }

    #[test]
    fn auto_sync_project_for_memory_defaults_on_for_project_memory() {
        let config = cli_test_config_with_gateway(None);
        let memory = Memory::new(
            "sync me".to_string(),
            None,
            Some("agent-memory".to_string()),
            None,
            None,
            Some("project".to_string()),
        );

        assert_eq!(
            auto_sync_project_for_memory(&config, &memory),
            Some("agent-memory")
        );
    }

    #[test]
    fn auto_sync_project_for_memory_respects_disabled_config() {
        let config = cli_test_config_with_gateway(Some(false));
        let memory = Memory::new(
            "do not sync me".to_string(),
            None,
            Some("agent-memory".to_string()),
            None,
            None,
            Some("project".to_string()),
        );

        assert_eq!(auto_sync_project_for_memory(&config, &memory), None);
    }

    #[test]
    fn auto_sync_project_for_memory_skips_global_and_unscoped_memories() {
        let config = cli_test_config_with_gateway(None);
        let global = Memory::new(
            "global".to_string(),
            None,
            Some(GLOBAL_PROJECT_IDENT.to_string()),
            None,
            None,
            Some("feedback".to_string()),
        );
        let unscoped = Memory::new(
            "unscoped".to_string(),
            None,
            None,
            None,
            None,
            Some("project".to_string()),
        );

        assert_eq!(auto_sync_project_for_memory(&config, &global), None);
        assert_eq!(auto_sync_project_for_memory(&config, &unscoped), None);
    }

    #[test]
    fn auto_sync_after_store_pushes_then_pulls_project_memories() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");
        let memory = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000201",
            "auto sync project memory",
            vec!["gateway", "auto-sync"],
            "agent-memory",
        );
        let (base_url, server) = spawn_auto_sync_gateway();
        let mut config = cli_test_config_with_gateway(None);
        config.gateway.base_url = Some(base_url);

        let (push, pull) =
            run_auto_sync_after_store(&conn, &config, "agent-memory").expect("auto sync");

        assert_eq!(push.candidates, 1);
        assert_eq!(push.pending, 1);
        assert_eq!(push.counts.created, 1);
        assert_eq!(pull.remote, 0);
        assert!(pull.cursor_updated);
        let sync = queries::get_memory_gateway_sync(&conn, &memory.id)
            .expect("load sync")
            .expect("sync metadata");
        assert_eq!(sync.gateway_memory_id, "gw-auto-1");
        assert_eq!(sync.last_seen_server_revision, 7);

        let requests = server.join().expect("gateway server");
        assert_eq!(requests.len(), 2);
        assert!(requests[0]
            .0
            .contains("POST /v1/projects/agent-memory/memories/push "));
        assert_eq!(requests[0].1["memories"].as_array().map(Vec::len), Some(1));
        assert!(requests[1]
            .0
            .contains("POST /v1/projects/agent-memory/memories/pull "));
        assert_eq!(requests[1].1["page_size"], json!(100));
    }

    fn insert_cli_test_memory(
        conn: &Connection,
        id: &str,
        content: &str,
        tags: Vec<&str>,
        project: &str,
    ) -> Memory {
        let mut memory = Memory::new(
            content.to_string(),
            Some(tags.into_iter().map(str::to_string).collect()),
            Some(project.to_string()),
            None,
            None,
            Some("project".to_string()),
        );
        memory.id = id.to_string();
        queries::insert_memory(conn, &memory).unwrap();
        memory
    }

    fn link_cli_test_memory(
        conn: &Connection,
        memory: &Memory,
        gateway_memory_id: &str,
        server_revision: i64,
    ) {
        queries::upsert_memory_gateway_sync(
            conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: memory.id.clone(),
                project: memory.project.clone().unwrap(),
                gateway_memory_id: gateway_memory_id.to_string(),
                last_seen_server_revision: server_revision,
                last_pushed_content_hash: Some(local_memory_hash(memory)),
                last_pulled_content_hash: None,
                sync_state: "created".to_string(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )
        .unwrap();
    }

    fn spawn_auto_sync_gateway() -> (String, std::thread::JoinHandle<Vec<(String, Value)>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind gateway");
        let addr = listener.local_addr().expect("gateway addr");
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept gateway request");
                let (first_line, body) = read_http_request(&mut stream);
                let value: Value = serde_json::from_str(&body).expect("request json");
                let response = if first_line.contains("/memories/push") {
                    let memory = value["memories"]
                        .as_array()
                        .and_then(|memories| memories.first())
                        .expect("push memory");
                    json!({
                        "project_ident": "agent-memory",
                        "server_revision": 7,
                        "results": [
                            {
                                "local_memory_id": memory["local_memory_id"],
                                "gateway_memory_id": "gw-auto-1",
                                "server_revision": 7,
                                "action": "created",
                                "content_hash": memory["content_hash"]
                            }
                        ]
                    })
                } else {
                    json!({
                        "project_ident": "agent-memory",
                        "server_revision": 7,
                        "memories": []
                    })
                };
                write_http_response(&mut stream, &response.to_string());
                requests.push((first_line, value));
            }
            requests
        });
        (format!("http://{addr}"), handle)
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> (String, String) {
        use std::io::Read;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set read timeout");
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).expect("read request");
            assert!(n > 0, "connection closed before headers");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(header_end) = find_bytes(&buf, b"\r\n\r\n") {
                let body_start = header_end + 4;
                let headers = String::from_utf8_lossy(&buf[..body_start]).to_string();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(key, value)| {
                            if key.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or(0);
                while buf.len() < body_start + content_length {
                    let n = stream.read(&mut tmp).expect("read request body");
                    assert!(n > 0, "connection closed before body");
                    buf.extend_from_slice(&tmp[..n]);
                }
                let first_line = headers.lines().next().unwrap_or_default().to_string();
                let body = String::from_utf8(buf[body_start..body_start + content_length].to_vec())
                    .expect("utf8 body");
                return (first_line, body);
            }
        }
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn write_http_response(stream: &mut std::net::TcpStream, body: &str) {
        use std::io::Write;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write response");
    }

    fn push_test_candidate(index: usize) -> PushCandidate {
        let local_memory_id = format!("local-{index}");
        PushCandidate {
            local_memory_id: local_memory_id.clone(),
            gateway_memory: GatewayMemory {
                project: "agent-memory".to_string(),
                content: format!("memory {index}"),
                memory_type: "project".to_string(),
                tags: vec!["gateway".to_string()],
                content_hash: format!("hash-{index}"),
                concept_hash: None,
                okf: None,
                local_memory_id: Some(local_memory_id),
                client_id: None,
                gateway_memory_id: None,
                base_server_revision: None,
                server_revision: None,
                created_at: None,
                updated_at: None,
                provenance: None,
                tombstone: None,
            },
            action: "create",
        }
    }

    fn cli_remote_memory(
        gateway_id: &str,
        content: &str,
        tags: Vec<&str>,
        revision: i64,
    ) -> GatewayMemory {
        let tags: Vec<String> = tags.into_iter().map(str::to_string).collect();
        GatewayMemory {
            project: "agent-memory".to_string(),
            content: content.to_string(),
            memory_type: "project".to_string(),
            content_hash: memory_content_hash(content, "project", &tags),
            concept_hash: None,
            okf: None,
            tags,
            local_memory_id: None,
            client_id: None,
            gateway_memory_id: Some(gateway_id.to_string()),
            base_server_revision: None,
            server_revision: Some(revision),
            created_at: None,
            updated_at: None,
            provenance: None,
            tombstone: None,
        }
    }

    #[test]
    fn gateway_okf_unknown_envelope_fields_survive_pull_noop_and_push() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");

        let document = agent_memory::okf::render_document(
            &agent_memory::okf::OkfConcept::minimal("knowledge", "portable body"),
            agent_memory::okf::RenderMode::Normalized,
        )
        .unwrap();
        let mut remote = cli_remote_memory("gw-okf", "portable body", vec!["sync"], 7);
        remote.concept_hash = Some("remote-semantic-hash".to_string());
        remote.okf = Some(crate::sync::GatewayOkfEnvelope {
            version: 1,
            format: "okf-markdown".to_string(),
            revision: 4,
            semantic_hash: "remote-semantic-hash".to_string(),
            document,
            extensions: std::collections::BTreeMap::from([(
                "x-future-proof".to_string(),
                serde_json::json!({"nested": [1, 2, 3]}),
            )]),
        });

        let (parsed, scope) = normalize_remote_okf("agent-memory", &remote, None).unwrap();
        let handler = agent_memory::okf::OkfDocumentHandler::new(&conn, scope);
        let first = handler
            .put_with_operation(None, &parsed, None, false, "gateway_pull")
            .unwrap();
        let stored = queries::get_memory_by_id(&conn, &first.id).unwrap();
        let outbound = gateway_okf_envelope(&conn, &stored).unwrap();
        assert_eq!(
            outbound.extensions.get("x-future-proof"),
            Some(&serde_json::json!({"nested": [1, 2, 3]}))
        );

        let (parsed_again, scope) =
            normalize_remote_okf("agent-memory", &remote, Some(&first.id)).unwrap();
        let noop = agent_memory::okf::OkfDocumentHandler::new(&conn, scope)
            .put_with_operation(Some(&first.id), &parsed_again, None, false, "gateway_pull")
            .unwrap();
        assert!(
            !noop.changed,
            "an identical pull must not create a revision"
        );
        assert_eq!(noop.revision, first.revision);
    }

    #[test]
    fn pull_planning_fast_forwards_only_when_local_matches_base_hash() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");

        let fast_forward = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000030",
            "shared base",
            vec!["fast"],
            "agent-memory",
        );
        let fast_forward_hash = local_memory_hash(&fast_forward);
        queries::upsert_memory_gateway_sync(
            &conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: fast_forward.id.clone(),
                project: "agent-memory".to_string(),
                gateway_memory_id: "gw-fast-forward".to_string(),
                last_seen_server_revision: 3,
                last_pushed_content_hash: None,
                last_pulled_content_hash: Some(fast_forward_hash),
                sync_state: "pulled".to_string(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )
        .unwrap();

        let local_edit = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000031",
            "locally edited",
            vec!["conflict"],
            "agent-memory",
        );
        let old_base_hash =
            memory_content_hash("old shared base", "project", &["conflict".to_string()]);
        queries::upsert_memory_gateway_sync(
            &conn,
            &MemoryGatewaySyncUpsert {
                local_memory_id: local_edit.id.clone(),
                project: "agent-memory".to_string(),
                gateway_memory_id: "gw-conflict".to_string(),
                last_seen_server_revision: 3,
                last_pushed_content_hash: None,
                last_pulled_content_hash: Some(old_base_hash),
                sync_state: "pulled".to_string(),
                tombstone_deleted: false,
                tombstone_at: None,
            },
        )
        .unwrap();

        let mut fast_remote =
            cli_remote_memory("gw-fast-forward", "remote update", vec!["fast"], 4);
        fast_remote.concept_hash = Some("different-additive-semantic-hash".to_string());
        let response = PullMemoriesResponse {
            project: "agent-memory".to_string(),
            memories: vec![
                fast_remote,
                cli_remote_memory("gw-conflict", "remote update", vec!["conflict"], 4),
            ],
            tombstones: vec![],
            next_cursor: Some("cursor-4".to_string()),
            server_revision: Some(4),
            has_more: false,
        };

        let plans = plan_pull_actions(&conn, "agent-memory", &response).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].action, "update");
        assert_eq!(
            plans[0].remote.concept_hash.as_deref(),
            Some("different-additive-semantic-hash"),
            "semantic hashes coexist while transition conflict planning remains legacy-compatible"
        );
        assert_eq!(
            plans[0].local_memory_id.as_deref(),
            Some(fast_forward.id.as_str())
        );
        assert_eq!(plans[1].action, "conflict");
        assert_eq!(
            plans[1].local_memory_id.as_deref(),
            Some(local_edit.id.as_str())
        );

        let conflicts =
            apply_pull_plans(&conn, &cli_test_config(), "agent-memory", &plans[1..]).unwrap();
        assert_eq!(conflicts, 1);
        let sync = queries::get_memory_gateway_sync(&conn, &local_edit.id)
            .unwrap()
            .expect("sync row");
        assert_eq!(sync.last_seen_server_revision, 3);
        assert_eq!(sync.sync_state, "pulled");
    }

    #[test]
    fn pull_planning_links_project_duplicate_but_not_global_duplicate() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        crate::db::run_migrations(&conn).expect("migrate");

        let project_duplicate = insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000040",
            "same project content",
            vec!["link"],
            "agent-memory",
        );
        insert_cli_test_memory(
            &conn,
            "aaaaaaaa-0000-1111-2222-000000000041",
            "global only content",
            vec!["global"],
            GLOBAL_PROJECT_IDENT,
        );

        let response = PullMemoriesResponse {
            project: "agent-memory".to_string(),
            memories: vec![
                cli_remote_memory("gw-link", "same project content", vec!["link"], 10),
                cli_remote_memory("gw-import", "global only content", vec!["global"], 11),
            ],
            tombstones: vec![],
            next_cursor: None,
            server_revision: Some(11),
            has_more: false,
        };

        let plans = plan_pull_actions(&conn, "agent-memory", &response).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].action, "link");
        assert_eq!(
            plans[0].local_memory_id.as_deref(),
            Some(project_duplicate.id.as_str())
        );
        assert_eq!(
            queries::get_memory_gateway_sync_by_gateway_id(&conn, "agent-memory", "gw-link")
                .unwrap(),
            None,
            "status planning must not write sync metadata"
        );
        assert_eq!(
            plans[1].action, "import",
            "global-scope matches must not satisfy project pull deconfliction"
        );
    }

    #[test]
    fn parse_working_get_project() {
        let cli =
            Cli::try_parse_from(["memory", "working", "get", "--project", "agent-memory"]).unwrap();
        match cli {
            Cli::Working {
                command: WorkingCommands::Get { project },
            } => assert_eq!(project.as_deref(), Some("agent-memory")),
            _ => panic!("expected Working::Get variant"),
        }
    }

    #[test]
    fn parse_working_set_stdin() {
        let cli =
            Cli::try_parse_from(["memory", "working", "set", "-", "-p", "agent-memory"]).unwrap();
        match cli {
            Cli::Working {
                command:
                    WorkingCommands::Set {
                        content,
                        file,
                        project,
                    },
            } => {
                assert_eq!(content.as_deref(), Some("-"));
                assert!(file.is_none());
                assert_eq!(project.as_deref(), Some("agent-memory"));
            }
            _ => panic!("expected Working::Set variant"),
        }
    }

    #[test]
    fn parse_working_clear_project() {
        let cli =
            Cli::try_parse_from(["memory", "working", "clear", "-p", "agent-memory"]).unwrap();
        match cli {
            Cli::Working {
                command: WorkingCommands::Clear { project },
            } => assert_eq!(project.as_deref(), Some("agent-memory")),
            _ => panic!("expected Working::Clear variant"),
        }
    }

    /// `resolve_boosts` with the normal path returns both current-project and
    /// global sentinel wired up with their respective multipliers.
    #[test]
    fn resolve_boosts_default_wires_both_scopes() {
        let boosts = resolve_boosts(None, Some("agent-memory"), false);
        assert_eq!(boosts.current_project, Some("agent-memory"));
        assert_eq!(boosts.global_project, Some(GLOBAL_PROJECT_IDENT));
        assert!((boosts.project_boost - PROJECT_BOOST).abs() < f32::EPSILON);
        assert!((boosts.global_boost - GLOBAL_BOOST).abs() < f32::EPSILON);
    }

    /// `--no-project-boost` must disable BOTH boosts so users get a genuinely
    /// flat ranking when they ask for one.
    #[test]
    fn resolve_boosts_disabled_kills_both() {
        let boosts = resolve_boosts(None, Some("agent-memory"), true);
        assert_eq!(boosts.current_project, None);
        assert_eq!(boosts.global_project, None);
        assert_eq!(boosts.project_boost, 1.0);
        assert_eq!(boosts.global_boost, 1.0);
    }

    /// Explicit `--project foo` overrides cwd-derived ident for the boost.
    #[test]
    fn resolve_boosts_explicit_project_overrides_cwd() {
        let boosts = resolve_boosts(Some("foo"), Some("agent-memory"), false);
        assert_eq!(boosts.current_project, Some("foo"));
    }

    /// Hint: with at least one global match, emit the directive-style count
    /// and suppress the zero-global reflection prompt.
    #[test]
    fn results_hint_global_present_emits_directive_count() {
        let h = results_hint(0, 2, 5, Some("agent-memory"), "what does the project do")
            .expect("hint should be present");
        assert!(h.contains("2 of 5 results are global-scope preferences"));
        assert!(!h.contains("No global-scope preferences matched"));
    }

    /// Hint: zero global matches during a scoped retrieval fires the
    /// reflection prompt asking the agent to pause.
    #[test]
    fn results_hint_zero_global_fires_reflection_prompt() {
        let h = results_hint(0, 0, 3, Some("agent-memory"), "what does the project do")
            .expect("hint should be present");
        assert!(h.contains("No global-scope preferences matched"));
    }

    /// Hint: cross-project + zero globals should include both the
    /// prior-art warning and the reflection prompt.
    #[test]
    fn results_hint_combines_cross_project_and_zero_global() {
        let h = results_hint(3, 0, 5, Some("agent-memory"), "what does the project do")
            .expect("hint should be present");
        assert!(h.contains("cross-project"));
        assert!(h.contains("No global-scope preferences matched"));
    }

    /// Hint: flat retrieval (no current-project) and no globals → no hint
    /// at all (nothing useful to say).
    #[test]
    fn results_hint_flat_ranking_no_globals_emits_nothing() {
        assert!(results_hint(0, 0, 5, None, "ramp up notes").is_none());
    }

    /// Hint: flat retrieval with globals still surfaces the directive-count
    /// — universal preferences are always worth flagging.
    #[test]
    fn results_hint_flat_ranking_with_globals_surfaces_count() {
        let h = results_hint(0, 1, 3, None, "ramp up notes").expect("hint should be present");
        assert!(h.contains("global-scope preferences"));
    }

    #[test]
    fn working_absence_hint_only_for_context_with_project() {
        let h = working_absence_hint(Some("agent-memory"), None, true).expect("hint");
        assert!(h.contains("No WorkingContext is set for project 'agent-memory'"));
        assert!(working_absence_hint(Some("agent-memory"), None, false).is_none());
        assert!(working_absence_hint(None, None, true).is_none());

        let ctx = WorkingContext {
            project: "agent-memory".to_string(),
            content: "handoff".to_string(),
            version: 1,
            updated_at: "2026-05-18T15:00:00Z".to_string(),
        };
        assert!(working_absence_hint(Some("agent-memory"), Some(&ctx), true).is_none());
    }

    /// `store_scope_hint` mentions both `--scope global` and the silent
    /// mis-classification risk (so the agent knows *why* to reclassify).
    #[test]
    fn store_scope_hint_mentions_global_and_risk() {
        let h = store_scope_hint();
        assert!(h.contains("--scope global"));
        assert!(h.contains("silent mis-classification"));
    }

    /// Canonical-state intent: a query mentioning git/version/tag fires the
    /// reminder regardless of project context.
    #[test]
    fn canonical_state_hint_fires_on_version_query() {
        let h = results_hint(
            0,
            1,
            3,
            Some("agent-memory"),
            "commit push and create new minor version patch tag for TraderX",
        )
        .expect("hint should be present");
        assert!(h.contains("git/version/release lookup"));
        assert!(h.contains("git log"));
        assert!(h.contains("how/why/what"));
    }

    /// Plain prose without canonical-state vocabulary must not trip the
    /// nudge — false positives degrade the signal.
    #[test]
    fn canonical_state_hint_silent_on_unrelated_query() {
        let h = results_hint(
            0,
            1,
            3,
            Some("agent-memory"),
            "how does the embedding cache work",
        )
        .expect("hint should be present");
        assert!(!h.contains("git/version/release lookup"));
    }

    /// Inflected forms (committing, tagged, versions) all match — the keyword
    /// scan compares whole tokens case-insensitively.
    #[test]
    fn canonical_state_intent_matches_inflections() {
        assert!(has_canonical_state_intent("we are tagging the release"));
        assert!(has_canonical_state_intent("Versions of the API"));
        assert!(has_canonical_state_intent("rebase onto main and push"));
    }

    /// Tokens that merely contain a keyword as a substring must not match —
    /// e.g. "stagger" should not be read as "tag".
    #[test]
    fn canonical_state_intent_requires_whole_token() {
        assert!(!has_canonical_state_intent("stagger the requests"));
        assert!(!has_canonical_state_intent("commitment to the user"));
        assert!(!has_canonical_state_intent("conversion of formats"));
    }

    #[test]
    fn store_quality_hint_reminds_agents_to_validate_content() {
        let h = store_quality_hint();
        assert!(h.contains("quality gate"));
        assert!(h.contains("reusable guidance"));
        assert!(h.contains("visible in git"));
        assert!(h.contains("configured pattern systems"));
    }

    #[test]
    fn scope_label_round_trip() {
        assert_eq!(scope_label(MemoryScope::Project), "project");
        assert_eq!(scope_label(MemoryScope::Global), "global");
    }

    // -- Light-XML output shape tests ---------------------------------------
    //
    // These replace the removed JSON-shape assertions. They exercise the same
    // render helpers the CLI dispatch layer prints, so we lock in the text
    // format without having to capture stdout from the `execute` function.

    use crate::db::models::Memory;
    use crate::render;

    fn mk_mem(id: &str, content: &str, project: Option<&str>) -> Memory {
        Memory {
            id: id.to_string(),
            content: content.to_string(),
            tags: None,
            project: project.map(String::from),
            agent: None,
            source_file: None,
            created_at: "2026-04-23T00:00:00Z".to_string(),
            updated_at: "2026-04-23T00:00:00Z".to_string(),
            access_count: 0,
            embedding: None,
            memory_type: Some("user".to_string()),
            content_raw: None,
            superseded_by: None,
            condenser_version: None,
            embedding_model: None,
        }
    }

    /// Store result should emit a single `<result status="stored" .../>` line
    /// carrying the short ID, scope, and project. No JSON braces anywhere.
    #[test]
    fn store_output_is_light_xml_result_line() {
        let s = render::render_action_result(
            "stored",
            &[
                (
                    "id",
                    render::short_id("abcdef12-3456-7890-abcd-ef1234567890").to_string(),
                ),
                ("scope", "global".to_string()),
                ("project", "__global__".to_string()),
            ],
        );
        assert_eq!(
            s,
            r#"<result status="stored" id="abcdef12" scope="global" project="__global__"/>"#
        );
        assert!(!s.contains('{'));
        assert!(!s.contains('}'));
    }

    /// Forget-by-id success emits `<result status="forgot" .../>` with the
    /// short ID. Not-found emits `status="not_found"` instead.
    #[test]
    fn forget_output_uses_forgot_status_on_success() {
        let s_ok = render::render_action_result("forgot", &[("id", "a4936eff".to_string())]);
        assert_eq!(s_ok, r#"<result status="forgot" id="a4936eff"/>"#);

        let s_miss = render::render_action_result("not_found", &[("id", "deadbeef".to_string())]);
        assert_eq!(s_miss, r#"<result status="not_found" id="deadbeef"/>"#);
    }

    /// Forget-by-query with no hits emits `no_matches`; with hits emits the
    /// count in the attribute set.
    #[test]
    fn forget_by_query_status_no_matches_vs_forgot() {
        assert_eq!(
            render::render_action_result("no_matches", &[]),
            r#"<result status="no_matches"/>"#
        );
        let s = render::render_action_result("forgot", &[("count", "3".to_string())]);
        assert_eq!(s, r#"<result status="forgot" count="3"/>"#);
    }

    /// Prune emits `<result status="pruned" count=".."/>` or `dry_run` when
    /// the --dry-run flag is passed. No longer carries the memory content list
    /// (callers can list + prune separately for a preview now).
    #[test]
    fn prune_output_uses_count_attribute() {
        let s = render::render_action_result("pruned", &[("count", "7".to_string())]);
        assert_eq!(s, r#"<result status="pruned" count="7"/>"#);
        let s_dry = render::render_action_result("dry_run", &[("count", "7".to_string())]);
        assert_eq!(s_dry, r#"<result status="dry_run" count="7"/>"#);
    }

    /// Memory get renders a `<memory>` wrapper with metadata attributes and
    /// the full content as inner text.
    #[test]
    fn get_output_is_memory_wrapper_with_full_content() {
        let m = mk_mem(
            "a4936eff-1234-5678-9abc-def012345678",
            "Content line\nwith newline",
            Some("agent-memory"),
        );
        let s = render::render_memory(&m);
        assert!(s.starts_with("<memory id=\"a4936eff\""));
        assert!(s.contains("project=\"agent-memory\""));
        assert!(s.contains("Content line\nwith newline"));
        assert!(s.ends_with("</memory>"));
    }

    /// Projects output uses a `<projects count=".."/>` block with lines
    /// marking the current project with `*`.
    #[test]
    fn projects_output_marks_current_project() {
        let rows = vec![
            (Some("agent-memory".to_string()), 42_i64),
            (Some("colorithmic".to_string()), 7),
        ];
        let s = render::render_projects(&rows, Some("agent-memory"));
        assert!(s.contains("<projects count=\"2\">"));
        assert!(s.contains("*agent-memory (42)"));
        assert!(s.contains(" colorithmic (7)"));
    }

    /// List empty projects surfaces a self-closing `<projects count="0"/>`.
    #[test]
    fn projects_output_empty_is_self_closing() {
        assert_eq!(
            render::render_projects(&[], None),
            "<projects count=\"0\"/>"
        );
    }

    /// Memory list renders `<memories count=".."/>` with the current-project
    /// marker when cwd matches.
    #[test]
    fn list_output_is_memory_list_block() {
        let mems = vec![
            mk_mem("11111111-aaaa", "local mem", Some("agent-memory")),
            mk_mem("22222222-bbbb", "other mem", Some("colorithmic")),
        ];
        let s = render::render_memory_list(&mems, Some("agent-memory"));
        assert!(s.contains("<memories count=\"2\">"));
        assert!(s.contains("1.*(user) agent-memory"));
        assert!(s.contains("2. (user) colorithmic"));
        assert!(s.ends_with("</memories>"));
    }

    /// Verify the output of every render helper contains zero JSON braces —
    /// the user's explicit rule ("JSON output goes away entirely").
    #[test]
    fn no_render_helper_emits_json_braces() {
        let m = mk_mem("11111111-aaaa-bbbb-cccc-dddddddddddd", "hi", Some("p"));
        let checks: Vec<String> = vec![
            render::render_action_result("stored", &[("id", "abc".to_string())]),
            render::render_hint("x"),
            render::render_memory(&m),
            render::render_memory_list(std::slice::from_ref(&m), None),
            render::render_projects(&[(Some("p".to_string()), 1)], None),
            render::render_ambiguous("abcd", std::slice::from_ref(&m)),
        ];
        for s in checks {
            assert!(!s.contains('{'), "unexpected '{{' in output: {s}");
            assert!(!s.contains('}'), "unexpected '}}' in output: {s}");
        }
    }

    #[test]
    fn okf_command_family_parses_complete_contract() {
        let commands = [
            vec!["memory", "okf", "validate", "-"],
            vec!["memory", "okf", "get", "abcd"],
            vec!["memory", "okf", "put", "new", "--dry-run"],
            vec!["memory", "okf", "read", "okf+memory://global/", "/index.md"],
            vec!["memory", "okf", "list", "okf+memory://global/"],
            vec![
                "memory",
                "okf",
                "index",
                "okf+memory://global/",
                "--tag",
                "x",
            ],
            vec!["memory", "okf", "log", "okf+memory://global/", "-k", "5"],
            vec!["memory", "okf", "history", "abcd", "-k", "5"],
            vec!["memory", "okf", "diff", "abcd", "1", "2"],
            vec!["memory", "okf", "graph", "abcd", "--direction", "both"],
            vec![
                "memory",
                "okf",
                "export",
                "okf+memory://global/",
                "target",
                "--dry-run",
            ],
            vec![
                "memory",
                "okf",
                "import",
                "source",
                "--scope",
                "global",
                "--dry-run",
            ],
        ];
        for args in commands {
            assert!(Cli::try_parse_from(args).is_ok());
        }
    }
}
