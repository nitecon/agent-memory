use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    AnnotateAble, ListResourcesResult, PaginatedRequestParams, RawResource,
    ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities,
    ServerInfo,
};
use rmcp::schemars;
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler};
use rusqlite::Connection;
use serde::Deserialize;

use crate::config::Config;
use crate::db::models::{Memory, WorkingContext};
use crate::db::queries::{self, ResolvedId};
use crate::embedding;
use crate::error::MemoryError;
use crate::project;
use crate::render;
use crate::search::{self, SearchOptions, SearchResult};

/// Score multiplier applied to memories tagged with the current project.
const PROJECT_BOOST: f32 = 1.5;
/// Score multiplier applied to memories tagged with the global-scope
/// sentinel project. Intentionally smaller than `PROJECT_BOOST` — see the
/// CLI module for the full tradeoff rationale.
const GLOBAL_BOOST: f32 = 1.25;
/// Reserved `project` ident for global-scoped memories. Kept in sync with
/// `crate::cli::GLOBAL_PROJECT_IDENT`. Re-declared here rather than
/// cross-imported so the MCP surface stays independent of the CLI module's
/// public API shape.
const GLOBAL_PROJECT_IDENT: &str = "__global__";
const STORE_QUALITY_HINT: &str = "Before relying on this memory, verify it passes the quality gate: it should preserve reusable guidance, a preference, procedure, non-obvious constraint, failure cause, or a pointer to the canonical system specified by user/repo/tool guidance plus why it matters. Prefer updating an existing related memory over creating a new one; if this only records state visible in git, repo files, CI, releases, tasks, comms, notes, or configured pattern systems, forget or rewrite it.";

#[derive(Clone)]
pub struct MemoryServer {
    tool_router: ToolRouter<Self>,
    conn: Arc<Mutex<Connection>>,
    config: Arc<Config>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StoreArgs {
    /// Memory content text to store
    pub content: String,
    /// Comma-separated tags for categorization
    pub tags: Option<String>,
    /// Project identifier. Defaults to the cwd-derived project ident if omitted.
    pub project: Option<String>,
    /// Agent identifier (e.g. "unreal-plugin-developer")
    pub agent: Option<String>,
    /// Source file path this memory relates to
    pub source_file: Option<String>,
    /// Memory type: user, feedback, project, reference
    pub memory_type: Option<String>,
    /// If true, store without any project tag (skips cwd auto-detection).
    pub no_project: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Search query (natural language or keywords)
    pub query: String,
    /// Maximum number of results to return (default: 10)
    pub limit: Option<usize>,
    /// Project to boost in ranking. Defaults to cwd-derived project ident.
    pub project: Option<String>,
    /// Hard filter: only return memories whose project equals this string.
    pub only: Option<String>,
    /// If true, disable the current-project boost entirely (flat ranking).
    pub no_project_boost: Option<bool>,
    /// Exact OKF concept type filter.
    pub concept_type: Option<String>,
    /// Exact tag filter.
    pub tag: Option<String>,
    /// Text-seeded graph expansion depth; 0 disables, default 1.
    pub graph_depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallArgs {
    /// Filter by project identifier
    pub project: Option<String>,
    /// Filter by agent identifier
    pub agent: Option<String>,
    /// Comma-separated tags to filter by
    pub tags: Option<String>,
    /// Filter by memory type: user, feedback, project, reference
    pub memory_type: Option<String>,
    /// Maximum number of results (default: 10)
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ForgetArgs {
    /// Specific memory ID to delete (full UUID or 4+ char short prefix)
    pub id: Option<String>,
    /// Search query to find and delete matching memories
    pub query: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PruneArgs {
    /// Maximum age in days (default: 90)
    pub max_age_days: Option<u64>,
    /// Minimum access count to keep (default: 0)
    pub min_access_count: Option<i64>,
    /// If true, show what would be pruned without deleting
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextArgs {
    /// Task description to find relevant memories for
    pub description: String,
    /// Maximum number of results (default: 5)
    pub limit: Option<usize>,
    /// Project to boost in ranking. Defaults to cwd-derived project ident.
    pub project: Option<String>,
    /// Hard filter: only return memories whose project equals this string.
    pub only: Option<String>,
    /// If true, disable the current-project boost entirely (flat ranking).
    pub no_project_boost: Option<bool>,
    /// Exact OKF concept type filter.
    pub concept_type: Option<String>,
    /// Exact tag filter.
    pub tag: Option<String>,
    /// Text-seeded graph expansion depth; 0 disables, default 1.
    pub graph_depth: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkingGetArgs {
    /// Project identifier. Defaults to the cwd-derived project ident if omitted.
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkingSetArgs {
    /// WorkingContext body to set for the project.
    pub content: String,
    /// Project identifier. Defaults to the cwd-derived project ident if omitted.
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkingClearArgs {
    /// Project identifier. Defaults to the cwd-derived project ident if omitted.
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetArgs {
    /// Memory IDs to fetch (full UUIDs or 4+ char short prefixes).
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveArgs {
    /// Move a single memory by ID (full UUID or 4+ char short prefix).
    pub id: Option<String>,
    /// Move all memories whose current project equals this value. Pass an
    /// empty string ("") to target memories with no project.
    pub from: Option<String>,
    /// New project ident. Pass an empty string ("") to clear the project tag.
    pub to: String,
    /// If true, return the would-be-affected memories without writing.
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CopyArgs {
    /// Copy a single memory by ID (full UUID or 4+ char short prefix).
    pub id: Option<String>,
    /// Copy all memories whose current project equals this value. Pass an
    /// empty string ("") to target memories with no project.
    pub from: Option<String>,
    /// New project ident for the copies. Pass an empty string ("") to create
    /// copies with no project tag.
    pub to: String,
    /// If true, return the would-be-affected memories without writing.
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OkfValidateArgs {
    /// Complete OKF Markdown document text.
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OkfGetArgs {
    /// Full memory ID, exact canonical memory URI, or /memories/<id>.md path.
    pub target: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OkfPutArgs {
    /// Virtual bundle URI controlling project/global/unscoped authorization.
    pub bundle: String,
    /// Existing ID/URI/path, or omit to allocate a new UUID.
    pub target: Option<String>,
    /// Complete OKF Markdown document. This tool mutates durable memory.
    pub content: String,
    /// Compare-and-swap revision; stale values reject without mutation.
    pub expected_revision: Option<i64>,
    /// Validate and diff without mutating durable memory.
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OkfListArgs {
    /// Virtual bundle URI.
    pub bundle: String,
    /// Virtual directory path, default /.
    pub path: Option<String>,
    /// Maximum entries, capped at 200.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OkfHistoryArgs {
    pub id: String,
    /// Maximum revisions, capped at 100.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OkfDiffArgs {
    pub id: String,
    pub revision_a: i64,
    pub revision_b: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OkfGraphArgs {
    pub target: String,
    pub relation: Option<String>,
    /// Traversal direction: in, out, or both.
    pub direction: Option<String>,
    /// Maximum traversal depth, hard-capped by the graph service.
    pub depth: Option<usize>,
    /// Maximum returned paths, capped at 1000.
    pub limit: Option<usize>,
}

impl MemoryServer {
    pub fn new(config: Config, conn: Connection) -> Self {
        Self {
            tool_router: Self::tool_router(),
            conn: Arc::new(Mutex::new(conn)),
            config: Arc::new(config),
        }
    }

    /// Format a `MemoryError` as a single light-XML `<result status="error" .../>`
    /// line so all MCP tool responses share the same shape. Kept local rather
    /// than in the render module because "error during tool call" is an
    /// MCP-specific concept.
    fn err_xml(e: MemoryError) -> String {
        render::render_action_result("error", &[("message", e.to_string())])
    }

    fn okf_err(error: impl std::fmt::Display) -> String {
        render::render_action_result("error", &[("message", error.to_string())])
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

    fn parse_okf_resource_uri(
        uri: &str,
    ) -> Result<(agent_memory::okf::BundleScope, String), String> {
        let (bundle, path) = if let Some(path) = uri.strip_prefix("okf+memory://global/") {
            ("okf+memory://global/".to_string(), path)
        } else if let Some(path) = uri.strip_prefix("okf+memory://unscoped/") {
            ("okf+memory://unscoped/".to_string(), path)
        } else if let Some(rest) = uri.strip_prefix("okf+memory://project/") {
            let Some((project, path)) = rest.split_once('/') else {
                return Err(format!("OKF resource URI has no virtual path: {uri}"));
            };
            (format!("okf+memory://project/{project}/"), path)
        } else {
            return Err(format!("unsupported OKF resource URI: {uri}"));
        };
        if path.is_empty() || path.contains('?') || path.contains('#') {
            return Err(format!("invalid OKF resource path in URI: {uri}"));
        }
        let scope = agent_memory::okf::BundleScope::parse_uri(&bundle)
            .map_err(|error| error.to_string())?;
        Ok((scope, format!("/{path}")))
    }

    fn read_okf_resource(&self, uri: &str) -> Result<String, String> {
        let (scope, path) = Self::parse_okf_resource_uri(uri)?;
        let conn = self
            .conn
            .lock()
            .map_err(|_| "memory database lock poisoned".to_string())?;
        agent_memory::okf::OkfBundleHandler::new(&conn, scope)
            .read(&path)
            .map(|entry| entry.content)
            .map_err(|error| error.to_string())
    }

    fn okf_resources(&self) -> Result<Vec<rmcp::model::Resource>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| "memory database lock poisoned".to_string())?;
        let rows = queries::list_projects(&conn).map_err(|error| error.to_string())?;
        let mut scopes = rows
            .into_iter()
            .map(|(project, _)| match project.as_deref() {
                Some(GLOBAL_PROJECT_IDENT) => agent_memory::okf::BundleScope::Global,
                Some(project) => agent_memory::okf::BundleScope::Project(project.to_string()),
                None => agent_memory::okf::BundleScope::Unscoped,
            })
            .collect::<Vec<_>>();
        scopes.sort_by_key(agent_memory::okf::BundleScope::uri);
        scopes.dedup_by(|left, right| left.uri() == right.uri());

        let mut resources = Vec::with_capacity(scopes.len() * 2);
        for scope in scopes {
            let root = scope.uri();
            for (path, name, description) in [
                (
                    "index.md",
                    "OKF memory index",
                    "Generated index of canonical OKF concepts",
                ),
                (
                    "log.md",
                    "OKF memory log",
                    "Generated immutable OKF revision log",
                ),
            ] {
                resources.push(
                    RawResource::new(format!("{root}{path}"), name)
                        .with_description(description)
                        .with_mime_type("text/markdown")
                        .no_annotation(),
                );
            }
        }
        Ok(resources)
    }

    fn resolve_working_project(explicit: Option<String>) -> Result<String, MemoryError> {
        let project = explicit
            .or_else(|| project::project_ident_from_cwd().ok())
            .ok_or_else(|| {
                MemoryError::Config(
                    "WorkingContext requires a project ident; run from a project or pass project"
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
}

#[tool_router]
impl MemoryServer {
    /// Store a memory with auto-embedding and BM25 indexing. Use this to save important context,
    /// user preferences, project decisions, feedback, or reference information for future retrieval.
    /// The project tag is auto-derived from the server's working directory unless overridden.
    /// Returns a <result status="stored" .../> light-XML line plus a quality-gate hint.
    #[tool(name = "memory_store")]
    fn store(&self, Parameters(args): Parameters<StoreArgs>) -> String {
        let tag_list = args
            .tags
            .map(|t| t.split(',').map(|s| s.trim().to_string()).collect());

        // Guard: the MCP surface does not yet expose a `scope` parameter,
        // so the only way to land in the global sentinel from MCP would be
        // to pass `project: "__global__"` explicitly — which we reject.
        // When MCP scope support ships this guard moves to "unless the
        // scope argument was `global`" identical to the CLI path.
        if args.project.as_deref() == Some(GLOBAL_PROJECT_IDENT) {
            return render::render_action_result(
                "error",
                &[(
                    "message",
                    format!(
                        "`{GLOBAL_PROJECT_IDENT}` is reserved for global-scoped memories. \
                         Use the CLI `memory store --scope global` until MCP exposes a scope argument."
                    ),
                )],
            );
        }

        let resolved_project = if args.no_project.unwrap_or(false) {
            None
        } else {
            args.project
                .or_else(|| project::project_ident_from_cwd().ok())
        };

        let mut memory = Memory::new(
            args.content.clone(),
            tag_list,
            resolved_project,
            args.agent,
            args.source_file,
            args.memory_type.or(Some("user".to_string())),
        );

        let emb = match embedding::embed_text(&args.content, &self.config.model_cache_dir) {
            Ok(e) => e,
            Err(e) => return Self::err_xml(e),
        };
        memory.embedding = Some(emb);

        let conn = self.conn.lock().unwrap();
        if let Err(e) = queries::insert_memory(&conn, &memory) {
            return Self::err_xml(e);
        }

        let mut attrs: Vec<(&str, String)> = vec![("id", render::short_id(&memory.id).to_string())];
        if let Some(p) = memory.project.as_deref() {
            attrs.push(("project", p.to_string()));
        }
        format!(
            "{}\n{}",
            render::render_action_result("stored", &attrs),
            render::render_hint(STORE_QUALITY_HINT)
        )
    }

    /// Hybrid BM25 + vector search across all memories. Combines keyword matching with semantic
    /// similarity. Boosts current-project memories by default while still surfacing cross-project
    /// results as prior-art. Returns a light-XML block with <project_memories>,
    /// <general_knowledge>, and <other_projects> sections.
    #[tool(name = "memory_search")]
    fn search(&self, Parameters(args): Parameters<SearchArgs>) -> String {
        let limit = args.limit.unwrap_or(10);

        let cwd_project = project::project_ident_from_cwd().ok();
        let boosts = resolve_boosts(
            args.project.as_deref(),
            cwd_project.as_deref(),
            args.no_project_boost.unwrap_or(false),
        );

        let opts = SearchOptions {
            limit,
            current_project: boosts.current_project,
            boost_factor: boosts.project_boost,
            only_project: args.only.as_deref(),
            global_project: boosts.global_project,
            global_boost_factor: boosts.global_boost,
            concept_type: args.concept_type.as_deref(),
            tag: args.tag.as_deref(),
            graph_depth: args.graph_depth.unwrap_or(1),
        };

        let conn = self.conn.lock().unwrap();
        let results =
            match search::hybrid_search(&conn, &args.query, opts, &self.config.model_cache_dir) {
                Ok(r) => r,
                Err(e) => return Self::err_xml(e),
            };

        render_ranked_xml(&results, boosts.current_project, None, false)
    }

    /// Filter memories by project, agent, tags, or memory type. Use for structured retrieval
    /// when you know the category of memory you're looking for. Returns a <memories count=".."/>
    /// light-XML block.
    #[tool(name = "memory_recall")]
    fn recall(&self, Parameters(args): Parameters<RecallArgs>) -> String {
        let limit = args.limit.unwrap_or(10);
        let tag_list: Option<Vec<String>> = args
            .tags
            .map(|t| t.split(',').map(|s| s.trim().to_string()).collect());

        let cwd_project = project::project_ident_from_cwd().ok();

        let conn = self.conn.lock().unwrap();
        let memories = match queries::list_memories(
            &conn,
            args.project.as_deref(),
            args.agent.as_deref(),
            tag_list.as_deref(),
            args.memory_type.as_deref(),
            limit,
        ) {
            Ok(m) => m,
            Err(e) => return Self::err_xml(e),
        };

        format!(
            "{}\n{}",
            render::render_memory_list(&memories, cwd_project.as_deref()),
            render::render_usage_legend()
        )
    }

    /// Remove memories by ID or by search query. Use with an ID (full UUID or short prefix)
    /// for precise deletion, or a query to find and remove matching memories. Ambiguous short
    /// prefixes return an <ambiguous/> block instead of deleting.
    #[tool(name = "memory_forget")]
    fn forget(&self, Parameters(args): Parameters<ForgetArgs>) -> String {
        let conn = self.conn.lock().unwrap();

        if let Some(id) = args.id {
            match queries::resolve_id_prefix(&conn, &id) {
                Ok(ResolvedId::Exact(full_id)) => {
                    let memory = match queries::get_memory_by_id(&conn, &full_id) {
                        Ok(memory) => memory,
                        Err(e) => return Self::err_xml(e),
                    };
                    if let Err(e) = crate::gateway_sync::tombstone_memory_before_local_removal(
                        &conn,
                        &self.config.gateway,
                        &memory,
                        "memory forgotten locally",
                    ) {
                        return Self::err_xml(e);
                    }
                    match queries::delete_memory(&conn, &full_id) {
                        Ok(true) => render::render_action_result(
                            "forgot",
                            &[("id", render::short_id(&full_id).to_string())],
                        ),
                        Ok(false) => render::render_action_result(
                            "not_found",
                            &[("id", render::short_id(&full_id).to_string())],
                        ),
                        Err(e) => Self::err_xml(e),
                    }
                }
                Ok(ResolvedId::Ambiguous(cands)) => render::render_ambiguous(&id, &cands),
                Ok(ResolvedId::NotFound) => {
                    render::render_action_result("not_found", &[("id", id)])
                }
                Err(e) => Self::err_xml(e),
            }
        } else if let Some(query) = args.query {
            let opts = SearchOptions::new(5);
            let results =
                match search::hybrid_search(&conn, &query, opts, &self.config.model_cache_dir) {
                    Ok(r) => r,
                    Err(e) => return Self::err_xml(e),
                };

            if results.is_empty() {
                return render::render_action_result("no_matches", &[]);
            }

            let mut deleted = 0usize;
            for r in &results {
                if let Err(e) = crate::gateway_sync::tombstone_memory_before_local_removal(
                    &conn,
                    &self.config.gateway,
                    &r.memory,
                    "memory forgotten locally",
                ) {
                    return Self::err_xml(e);
                }
                if let Ok(true) = queries::delete_memory(&conn, &r.memory.id) {
                    deleted += 1;
                }
            }
            render::render_action_result("forgot", &[("count", deleted.to_string())])
        } else {
            render::render_action_result(
                "error",
                &[(
                    "message",
                    "Either 'id' or 'query' must be provided".to_string(),
                )],
            )
        }
    }

    /// Decay stale, low-access memories. Removes memories older than max_age_days with
    /// access_count at or below min_access_count. Use dry_run to preview before deleting.
    /// Returns a <result status="pruned" count=".."/> line.
    #[tool(name = "memory_prune")]
    fn prune(&self, Parameters(args): Parameters<PruneArgs>) -> String {
        let max_age = args.max_age_days.unwrap_or(90);
        let min_access = args.min_access_count.unwrap_or(0);
        let dry_run = args.dry_run.unwrap_or(false);

        let conn = self.conn.lock().unwrap();
        let pruned = match queries::prune_memories(&conn, max_age, min_access, dry_run) {
            Ok(p) => p,
            Err(e) => return Self::err_xml(e),
        };

        let status = if dry_run { "dry_run" } else { "pruned" };
        render::render_action_result(status, &[("count", pruned.len().to_string())])
    }

    /// Return the most relevant memories for a given task description, with a current-project
    /// boost so local context wins ties but cross-project memories can still surface as prior-art.
    /// Use at the start of a task. Returns a light-XML block grouped into <project_memories>,
    /// <general_knowledge>, and <other_projects> sections.
    #[tool(name = "memory_context")]
    fn context(&self, Parameters(args): Parameters<ContextArgs>) -> String {
        let limit = args.limit.unwrap_or(5);

        let cwd_project = project::project_ident_from_cwd().ok();
        let boosts = resolve_boosts(
            args.project.as_deref(),
            cwd_project.as_deref(),
            args.no_project_boost.unwrap_or(false),
        );

        let opts = SearchOptions {
            limit,
            current_project: boosts.current_project,
            boost_factor: boosts.project_boost,
            only_project: args.only.as_deref(),
            global_project: boosts.global_project,
            global_boost_factor: boosts.global_boost,
            concept_type: args.concept_type.as_deref(),
            tag: args.tag.as_deref(),
            graph_depth: args.graph_depth.unwrap_or(1),
        };

        let conn = self.conn.lock().unwrap();
        let results = match search::hybrid_search(
            &conn,
            &args.description,
            opts,
            &self.config.model_cache_dir,
        ) {
            Ok(r) => r,
            Err(e) => return Self::err_xml(e),
        };

        let working_context = match boosts.current_project {
            Some(project) => match queries::get_working_context(&conn, project) {
                Ok(ctx) => ctx,
                Err(e) => return Self::err_xml(e),
            },
            None => None,
        };

        render_ranked_xml(
            &results,
            boosts.current_project,
            working_context.as_ref(),
            true,
        )
    }

    /// Return the current project's WorkingContext handoff. The response always succeeds for a
    /// valid project: present="true" includes full content, present="false" means no handoff row
    /// exists.
    #[tool(name = "memory_working_get")]
    fn working_get(&self, Parameters(args): Parameters<WorkingGetArgs>) -> String {
        let project = match Self::resolve_working_project(args.project) {
            Ok(project) => project,
            Err(e) => return Self::err_xml(e),
        };

        let conn = self.conn.lock().unwrap();
        match queries::get_working_context(&conn, &project) {
            Ok(ctx) => render::render_working_context(ctx.as_ref(), &project),
            Err(e) => Self::err_xml(e),
        }
    }

    /// Replace the current project's WorkingContext handoff. The content limit is 65,536
    /// characters.
    #[tool(name = "memory_working_set")]
    fn working_set(&self, Parameters(args): Parameters<WorkingSetArgs>) -> String {
        let project = match Self::resolve_working_project(args.project) {
            Ok(project) => project,
            Err(e) => return Self::err_xml(e),
        };

        let conn = self.conn.lock().unwrap();
        match queries::set_working_context(&conn, &project, &args.content) {
            Ok(ctx) => render::render_action_result(
                "working_context_set",
                &[
                    ("project", ctx.project),
                    ("version", ctx.version.to_string()),
                    ("updated_at", ctx.updated_at),
                ],
            ),
            Err(e) => Self::err_xml(e),
        }
    }

    /// Clear the current project's WorkingContext handoff by deleting the row. Idempotent when no
    /// row exists.
    #[tool(name = "memory_working_clear")]
    fn working_clear(&self, Parameters(args): Parameters<WorkingClearArgs>) -> String {
        let project = match Self::resolve_working_project(args.project) {
            Ok(project) => project,
            Err(e) => return Self::err_xml(e),
        };

        let conn = self.conn.lock().unwrap();
        match queries::clear_working_context(&conn, &project) {
            Ok(deleted) => render::render_action_result(
                "working_context_cleared",
                &[("project", project), ("deleted", deleted.to_string())],
            ),
            Err(e) => Self::err_xml(e),
        }
    }

    /// Fetch full content for one or more memory IDs. Each ID can be a full UUID or a 4+ char
    /// short prefix; ambiguous prefixes return an <ambiguous/> block. Pair with memory_search
    /// or memory_context for a two-stage retrieval: scan previews, then pull full content on
    /// the handful you want to read.
    #[tool(name = "memory_get")]
    fn get(&self, Parameters(args): Parameters<GetArgs>) -> String {
        let conn = self.conn.lock().unwrap();
        let mut fetched: Vec<Memory> = Vec::with_capacity(args.ids.len());
        let mut out_parts: Vec<String> = Vec::with_capacity(args.ids.len());

        for id in &args.ids {
            match queries::resolve_id_prefix(&conn, id) {
                Ok(ResolvedId::Exact(full_id)) => {
                    match queries::get_memory_by_id(&conn, &full_id) {
                        Ok(m) => {
                            out_parts.push(render::render_memory(&m));
                            fetched.push(m);
                        }
                        Err(MemoryError::NotFound(_)) => {
                            out_parts.push(render::render_action_result(
                                "not_found",
                                &[("id", id.clone())],
                            ));
                        }
                        Err(e) => return Self::err_xml(e),
                    }
                }
                Ok(ResolvedId::Ambiguous(cands)) => {
                    out_parts.push(render::render_ambiguous(id, &cands));
                }
                Ok(ResolvedId::NotFound) => {
                    out_parts.push(render::render_action_result(
                        "not_found",
                        &[("id", id.clone())],
                    ));
                }
                Err(e) => return Self::err_xml(e),
            }
        }

        let hit_ids: Vec<String> = fetched.iter().map(|m| m.id.clone()).collect();
        if let Err(e) = queries::increment_access(&conn, &hit_ids) {
            return Self::err_xml(e);
        }

        out_parts.join("\n")
    }

    /// Reassign the project ident on one or more memories. Use this to migrate memories
    /// tagged under a legacy project name to the canonical git-remote ident the cwd resolver
    /// returns now. Pass `id` for a single memory (full UUID or short prefix) or `from` to
    /// bulk-rename. A project-wide move transfers that project's WorkingContext to the target;
    /// moving to an empty target clears it. Empty strings on `from`/`to` target or assign a
    /// NULL project. Set `dry_run: true` to preview without writing.
    #[tool(name = "memory_move")]
    fn move_tool(&self, Parameters(args): Parameters<MoveArgs>) -> String {
        // Mirror the CLI guard: reassigning into the global sentinel via
        // `to: "__global__"` would bypass the `--scope global` contract.
        if args.to == GLOBAL_PROJECT_IDENT {
            return render::render_action_result(
                "error",
                &[(
                    "message",
                    format!(
                        "`{GLOBAL_PROJECT_IDENT}` is reserved for global-scoped memories. \
                         Use the CLI `memory store --scope global` for new globals, or \
                         re-run with a different `to` value."
                    ),
                )],
            );
        }
        let to = empty_to_none_owned(&args.to);
        let dry_run = args.dry_run.unwrap_or(false);
        let conn = self.conn.lock().unwrap();

        // Resolve the id (if any) through the prefix resolver so MCP callers
        // can pass short UUIDs just like the CLI.
        let resolved_id = match args.id.as_deref() {
            None => None,
            Some(raw) => match queries::resolve_id_prefix(&conn, raw) {
                Ok(ResolvedId::Exact(full)) => Some(full),
                Ok(ResolvedId::Ambiguous(cands)) => {
                    return render::render_ambiguous(raw, &cands);
                }
                Ok(ResolvedId::NotFound) => {
                    return render::render_action_result("not_found", &[("id", raw.to_string())]);
                }
                Err(e) => return Self::err_xml(e),
            },
        };

        run_move(
            &conn,
            resolved_id.as_deref(),
            args.from.as_deref(),
            to.as_deref(),
            dry_run,
        )
    }

    /// Duplicate one or more memories under a new project ident. WorkingContext is not copied.
    /// Preserves content, tags,
    /// agent, source_file, memory_type, and the cached embedding; a new UUID is minted and
    /// timestamps reset. Pass `id` for a single memory (full UUID or short prefix) or `from`
    /// to bulk-copy. Empty strings on `from`/`to` target or assign a NULL project. Set
    /// `dry_run: true` to preview without writing.
    #[tool(name = "memory_copy")]
    fn copy_tool(&self, Parameters(args): Parameters<CopyArgs>) -> String {
        let to = empty_to_none_owned(&args.to);
        let dry_run = args.dry_run.unwrap_or(false);
        let conn = self.conn.lock().unwrap();

        let resolved_id = match args.id.as_deref() {
            None => None,
            Some(raw) => match queries::resolve_id_prefix(&conn, raw) {
                Ok(ResolvedId::Exact(full)) => Some(full),
                Ok(ResolvedId::Ambiguous(cands)) => {
                    return render::render_ambiguous(raw, &cands);
                }
                Ok(ResolvedId::NotFound) => {
                    return render::render_action_result("not_found", &[("id", raw.to_string())]);
                }
                Err(e) => return Self::err_xml(e),
            },
        };

        run_copy(
            &conn,
            resolved_id.as_deref(),
            args.from.as_deref(),
            to.as_deref(),
            dry_run,
        )
    }

    /// List distinct durable-memory project idents with memory counts. Useful for spotting alias
    /// mismatches before running memory_move. The current cwd-derived project is marked with `*`.
    /// Returns a <projects count=".."/> light-XML block.
    #[tool(name = "memory_projects")]
    fn projects(&self) -> String {
        let cwd_project = project::project_ident_from_cwd().ok();
        let conn = self.conn.lock().unwrap();
        let rows = match queries::list_projects(&conn) {
            Ok(r) => r,
            Err(e) => return Self::err_xml(e),
        };
        render::render_projects(&rows, cwd_project.as_deref())
    }

    /// Validate bounded OKF Markdown without mutating memory.
    #[tool(name = "memory_okf_validate")]
    fn okf_validate(&self, Parameters(args): Parameters<OkfValidateArgs>) -> String {
        match agent_memory::okf::parse_document(&args.content) {
            Ok(parsed) => render::render_action_result(
                "okf_valid",
                &[("diagnostics", parsed.diagnostics.len().to_string())],
            ),
            Err(error) => Self::okf_err(error),
        }
    }

    /// Render one canonical durable memory as complete OKF Markdown. WorkingContext is never
    /// addressable through this tool.
    #[tool(name = "memory_okf_get")]
    fn okf_get(&self, Parameters(args): Parameters<OkfGetArgs>) -> String {
        let conn = self.conn.lock().unwrap();
        let id = match Self::resolve_okf_target(&conn, &args.target) {
            Ok(id) => id,
            Err(error) => return Self::err_xml(error),
        };
        let scope = match Self::okf_scope_for_id(&conn, &id) {
            Ok(scope) => scope,
            Err(error) => return Self::err_xml(error),
        };
        match agent_memory::okf::OkfDocumentHandler::new(&conn, scope).render(&id) {
            Ok(document) => document.text,
            Err(error) => Self::okf_err(error),
        }
    }

    /// MUTATES DURABLE MEMORY: create or update a canonical OKF concept from explicit complete
    /// document content. Use expected_revision for compare-and-swap safety; dry_run performs no
    /// mutation. The bundle URI explicitly authorizes project/global/unscoped scope.
    #[tool(name = "memory_okf_put")]
    fn okf_put(&self, Parameters(args): Parameters<OkfPutArgs>) -> String {
        let parsed = match agent_memory::okf::parse_document(&args.content) {
            Ok(parsed) => parsed,
            Err(error) => return Self::okf_err(error),
        };
        let scope = match agent_memory::okf::BundleScope::parse_uri(&args.bundle) {
            Ok(scope) => scope,
            Err(error) => return Self::okf_err(error),
        };
        let dry_run = args.dry_run.unwrap_or(false);
        let embedding = if dry_run {
            None
        } else {
            match embedding::embed_text(&parsed.concept.body, &self.config.model_cache_dir) {
                Ok(value) => Some(value),
                Err(error) => return Self::err_xml(error),
            }
        };
        let conn = self.conn.lock().unwrap();
        let result = match agent_memory::okf::OkfDocumentHandler::new(&conn, scope).put(
            args.target.as_deref(),
            &parsed,
            args.expected_revision,
            dry_run,
        ) {
            Ok(result) => result,
            Err(error) => return Self::okf_err(error),
        };
        if let Some(embedding) = embedding {
            let blob = crate::db::models::embedding_to_blob(&embedding);
            if let Err(error) = conn.execute(
                "UPDATE memories SET embedding = ?1, embedding_model = ?2 WHERE id = ?3",
                rusqlite::params![
                    blob,
                    crate::db::models::EMBEDDING_MODEL_NAME_DEFAULT,
                    result.id
                ],
            ) {
                return Self::err_xml(MemoryError::from(error));
            }
        }
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
            ],
        )
    }

    /// List a bounded virtual OKF bundle directory. Generated index/log paths are read-only and
    /// WorkingContext is never included.
    #[tool(name = "memory_okf_list")]
    fn okf_list(&self, Parameters(args): Parameters<OkfListArgs>) -> String {
        let scope = match agent_memory::okf::BundleScope::parse_uri(&args.bundle) {
            Ok(scope) => scope,
            Err(error) => return Self::okf_err(error),
        };
        let conn = self.conn.lock().unwrap();
        let entries = match agent_memory::okf::OkfBundleHandler::new(&conn, scope)
            .list(args.path.as_deref().unwrap_or("/"))
        {
            Ok(entries) => entries,
            Err(error) => return Self::okf_err(error),
        };
        let limit = args.limit.unwrap_or(100).clamp(1, 200);
        entries
            .into_iter()
            .take(limit)
            .map(|entry| {
                render::render_action_result(
                    "okf_entry",
                    &[
                        ("path", entry.path),
                        ("kind", format!("{:?}", entry.kind).to_ascii_lowercase()),
                        ("read_only", entry.read_only.to_string()),
                    ],
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Return bounded immutable revision metadata for one durable memory.
    #[tool(name = "memory_okf_history")]
    fn okf_history(&self, Parameters(args): Parameters<OkfHistoryArgs>) -> String {
        let conn = self.conn.lock().unwrap();
        let id = match Self::resolve_okf_target(&conn, &args.id) {
            Ok(id) => id,
            Err(error) => return Self::err_xml(error),
        };
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        let mut stmt = match conn.prepare(
            "SELECT revision, operation, actor, content_hash, created_at
             FROM memory_revisions WHERE memory_id = ?1
             ORDER BY revision DESC LIMIT ?2",
        ) {
            Ok(stmt) => stmt,
            Err(error) => return Self::err_xml(error.into()),
        };
        let rows = match stmt.query_map(rusqlite::params![id, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        }) {
            Ok(rows) => match rows.collect::<Result<Vec<_>, _>>() {
                Ok(rows) => rows,
                Err(error) => return Self::err_xml(error.into()),
            },
            Err(error) => return Self::err_xml(error.into()),
        };
        rows.into_iter()
            .map(|(revision, operation, actor, hash, at)| {
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
                render::render_action_result("okf_revision", &attrs)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Compare two immutable semantic revision snapshots with bounded field summaries.
    #[tool(name = "memory_okf_diff")]
    fn okf_diff(&self, Parameters(args): Parameters<OkfDiffArgs>) -> String {
        let conn = self.conn.lock().unwrap();
        let id = match Self::resolve_okf_target(&conn, &args.id) {
            Ok(id) => id,
            Err(error) => return Self::err_xml(error),
        };
        let left = match mcp_revision_snapshot(&conn, &id, args.revision_a) {
            Ok(value) => value,
            Err(error) => return Self::err_xml(error),
        };
        let right = match mcp_revision_snapshot(&conn, &id, args.revision_b) {
            Ok(value) => value,
            Err(error) => return Self::err_xml(error),
        };
        let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
            return Self::okf_err("revision snapshot is not an object");
        };
        left.keys()
            .chain(right.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|field| left.get(field) != right.get(field))
            .map(|field| {
                render::render_action_result(
                    "okf_diff",
                    &[
                        ("id", id.clone()),
                        ("field", field.clone()),
                        (
                            "before",
                            mcp_bounded_value(left.get(&field).map(ToString::to_string)),
                        ),
                        (
                            "after",
                            mcp_bounded_value(right.get(&field).map(ToString::to_string)),
                        ),
                    ],
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Traverse bounded typed memory relationships with direction, relation, depth, cycles,
    /// paths, and unresolved-reference diagnostics.
    #[tool(name = "memory_okf_graph")]
    fn okf_graph(&self, Parameters(args): Parameters<OkfGraphArgs>) -> String {
        use crate::concepts::graph::{self, Direction, TraversalOptions};

        let conn = self.conn.lock().unwrap();
        let id = match Self::resolve_okf_target(&conn, &args.target) {
            Ok(id) => id,
            Err(error) => return Self::err_xml(error),
        };
        let direction = match args.direction.as_deref().unwrap_or("out") {
            "in" => Direction::Incoming,
            "out" => Direction::Outgoing,
            "both" => Direction::Both,
            other => return Self::okf_err(format!("invalid graph direction `{other}`")),
        };
        let result = match graph::traverse(
            &conn,
            std::slice::from_ref(&id),
            &TraversalOptions {
                direction,
                relations: args.relation.into_iter().collect(),
                max_depth: args.depth.unwrap_or(2),
                max_results: args.limit.unwrap_or(100),
                ..TraversalOptions::default()
            },
        ) {
            Ok(result) => result,
            Err(error) => return Self::err_xml(error),
        };
        let mut lines = result
            .paths
            .into_iter()
            .map(|path| {
                render::render_action_result(
                    "okf_graph_path",
                    &[
                        ("root", id.clone()),
                        ("path", path.nodes.join(" -> ")),
                        ("depth", path.steps.len().to_string()),
                        ("cycle", path.cycle.to_string()),
                    ],
                )
            })
            .collect::<Vec<_>>();
        lines.extend(result.diagnostics.into_iter().map(|diagnostic| {
            render::render_action_result(
                "okf_graph_unresolved",
                &[
                    ("source", diagnostic.source),
                    ("reference", diagnostic.reference),
                    ("relation", diagnostic.relation),
                ],
            )
        }));
        lines.join("\n")
    }
}

fn mcp_revision_snapshot(
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

fn mcp_bounded_value(value: Option<String>) -> String {
    let value = value.unwrap_or_else(|| "null".to_string());
    let mut chars = value.chars();
    let preview = chars.by_ref().take(240).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn empty_to_none_owned(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
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

/// Execute a move operation and return a light-XML string. Shared between the
/// MCP move tool and the would-be CLI path; duplicated here to keep the MCP
/// module compilable without pulling in cli internals.
fn run_move(
    conn: &Connection,
    id: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    dry_run: bool,
) -> String {
    match (id, from) {
        (Some(id), _) => {
            if dry_run {
                match queries::get_memory_by_id(conn, id) {
                    Ok(mem) => {
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
                        render::render_action_result("dry_run", &attrs)
                    }
                    Err(e) => MemoryServer::err_xml(e),
                }
            } else {
                match queries::move_memory_by_id(conn, id, to) {
                    Ok(changed) => {
                        let status = if changed { "moved" } else { "not_found" };
                        let mut attrs: Vec<(&str, String)> =
                            vec![("id", render::short_id(id).to_string())];
                        if let Some(t) = to {
                            attrs.push(("to_project", t.to_string()));
                        }
                        render::render_action_result(status, &attrs)
                    }
                    Err(e) => MemoryServer::err_xml(e),
                }
            }
        }
        (None, Some(from)) => {
            let from_opt = if from.is_empty() { None } else { Some(from) };
            if dry_run {
                match queries::list_memories_by_project(conn, from_opt) {
                    Ok(mems) => {
                        let working_context = match working_context_move_preview(conn, from_opt, to)
                        {
                            Ok(state) => state,
                            Err(e) => return MemoryServer::err_xml(e),
                        };
                        let mut attrs: Vec<(&str, String)> = vec![
                            ("would_move", mems.len().to_string()),
                            ("from_project", from_opt.unwrap_or("").to_string()),
                            ("working_context", working_context.to_string()),
                        ];
                        if let Some(t) = to {
                            attrs.push(("to_project", t.to_string()));
                        }
                        render::render_action_result("dry_run", &attrs)
                    }
                    Err(e) => MemoryServer::err_xml(e),
                }
            } else {
                let working_context =
                    match move_working_context_for_project_move(conn, from_opt, to) {
                        Ok(state) => state,
                        Err(e) => return MemoryServer::err_xml(e),
                    };
                match queries::move_memories_by_project(conn, from_opt, to) {
                    Ok(count) => {
                        let mut attrs: Vec<(&str, String)> = vec![
                            ("count", count.to_string()),
                            ("from_project", from_opt.unwrap_or("").to_string()),
                            ("working_context", working_context.as_str().to_string()),
                        ];
                        if let Some(t) = to {
                            attrs.push(("to_project", t.to_string()));
                        }
                        render::render_action_result("moved", &attrs)
                    }
                    Err(e) => MemoryServer::err_xml(e),
                }
            }
        }
        (None, None) => render::render_action_result(
            "error",
            &[(
                "message",
                "Either 'id' or 'from' must be provided".to_string(),
            )],
        ),
    }
}

/// Execute a copy operation and return a light-XML string. See [`run_move`] for
/// the contract; this is its copy-flavored twin.
fn run_copy(
    conn: &Connection,
    id: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    dry_run: bool,
) -> String {
    match (id, from) {
        (Some(id), _) => {
            if dry_run {
                match queries::get_memory_by_id(conn, id) {
                    Ok(mem) => {
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
                        render::render_action_result("dry_run", &attrs)
                    }
                    Err(e) => MemoryServer::err_xml(e),
                }
            } else {
                match queries::copy_memory_by_id(conn, id, to) {
                    Ok(new_id) => {
                        let mut attrs: Vec<(&str, String)> = vec![
                            ("source_id", render::short_id(id).to_string()),
                            ("new_id", render::short_id(&new_id).to_string()),
                        ];
                        if let Some(t) = to {
                            attrs.push(("to_project", t.to_string()));
                        }
                        render::render_action_result("copied", &attrs)
                    }
                    Err(e) => MemoryServer::err_xml(e),
                }
            }
        }
        (None, Some(from)) => {
            let from_opt = if from.is_empty() { None } else { Some(from) };
            if dry_run {
                match queries::list_memories_by_project(conn, from_opt) {
                    Ok(mems) => {
                        let mut attrs: Vec<(&str, String)> = vec![
                            ("would_copy", mems.len().to_string()),
                            ("from_project", from_opt.unwrap_or("").to_string()),
                        ];
                        if let Some(t) = to {
                            attrs.push(("to_project", t.to_string()));
                        }
                        render::render_action_result("dry_run", &attrs)
                    }
                    Err(e) => MemoryServer::err_xml(e),
                }
            } else {
                match queries::copy_memories_by_project(conn, from_opt, to) {
                    Ok(new_ids) => {
                        let mut attrs: Vec<(&str, String)> = vec![
                            ("count", new_ids.len().to_string()),
                            ("from_project", from_opt.unwrap_or("").to_string()),
                        ];
                        if let Some(t) = to {
                            attrs.push(("to_project", t.to_string()));
                        }
                        render::render_action_result("copied", &attrs)
                    }
                    Err(e) => MemoryServer::err_xml(e),
                }
            }
        }
        (None, None) => render::render_action_result(
            "error",
            &[(
                "message",
                "Either 'id' or 'from' must be provided".to_string(),
            )],
        ),
    }
}

/// Mirror of `crate::cli::BoostConfig`. The two modules compute boosts
/// the same way; duplicating the struct keeps each surface self-contained
/// without introducing a cross-module coupling that serves nothing else.
struct BoostConfig<'a> {
    current_project: Option<&'a str>,
    global_project: Option<&'a str>,
    project_boost: f32,
    global_boost: f32,
}

fn resolve_boosts<'a>(
    explicit: Option<&'a str>,
    cwd: Option<&'a str>,
    disable: bool,
) -> BoostConfig<'a> {
    if disable {
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

/// Render a ranked result set as light-XML with the same reflection hint the
/// CLI emits. Factored out so `search` and `context` share the rendering path.
/// Also appends the `<usage>` legend at the bottom so MCP callers get the
/// same short-ID / section-tag guidance the CLI emits — ships unconditionally
/// (even on zero-result runs) because that's when new callers need it most.
fn render_ranked_xml(
    results: &[SearchResult],
    current_project: Option<&str>,
    working_context: Option<&WorkingContext>,
    include_working_absence_hint: bool,
) -> String {
    let total = results.len();
    let globals = results.iter().filter(|r| r.is_global).count();
    let cross = if current_project.is_some() {
        results.iter().filter(|r| !r.is_current_project).count()
    } else {
        0
    };
    let hint = combine_hints(
        results_hint(cross, globals, total, current_project),
        working_absence_hint(
            current_project,
            working_context,
            include_working_absence_hint,
        ),
    );
    let rendered =
        render::render_search_results(results, current_project, working_context, hint.as_deref());
    let body = if rendered.is_empty() {
        "<results count=\"0\"/>".to_string()
    } else {
        rendered
    };
    format!("{body}\n{}", render::render_usage_legend())
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
                "No WorkingContext is set for project '{project}'. If pausing substantial active work, use memory_working_set to leave a handoff."
            )
        })
    } else {
        None
    }
}

/// See `crate::cli::results_hint` for the full doc; kept in sync verbatim.
fn results_hint(
    cross: usize,
    globals: usize,
    total: usize,
    current: Option<&str>,
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
                    "{cross} of {total} results are cross-project (is_current_project=false). Use those as prior-art or general guidance, not direct context for '{cp}'. Use memory_get for full content."
                )
            });
        }
    }

    if globals > 0 {
        parts.push(format!(
            "{globals} of {total} results are global-scope preferences (apply across all projects). Treat them as directives, not suggestions."
        ));
    } else if current.is_some() {
        parts.push(
            "No global-scope preferences matched this task. If the user has stated a general rule relevant to this domain, it did not surface — consider asking before acting if you suspect one exists."
                .to_string(),
        );
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

#[tool_handler]
impl ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(
            "Persistent memory system for AI coding agents. All tools return light-XML \
                 text (sectioned tags with numbered content lines — no JSON). \
                 memory_store saves context (auto-tagged with cwd project), \
                 memory_search and memory_context rank with a current-project boost and \
                 return grouped <project_memories>/<general_knowledge>/<other_projects> \
                 sections; fetch full content via memory_get. \
                 memory_recall filters by project/agent/tags/type, \
                 memory_forget deletes by id (short prefix supported) or query, \
                 memory_prune cleans stale memories. memory_working_get/set/clear manages the \
                 current project's transient WorkingContext handoff. memory_projects lists distinct durable-memory \
                 idents (spot aliases), memory_move reassigns the project ident on memories \
                 (project-wide moves also transfer or clear WorkingContext), memory_copy duplicates memories \
                 under a new ident without copying WorkingContext.",
        )
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        std::future::ready(
            self.okf_resources()
                .map(ListResourcesResult::with_all_items)
                .map_err(|error| McpError::internal_error(error, None)),
        )
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
        let uri = request.uri;
        std::future::ready(
            self.read_okf_resource(&uri)
                .map(|text| {
                    ReadResourceResult::new(vec![
                        ResourceContents::text(text, uri).with_mime_type("text/markdown")
                    ])
                })
                .map_err(|error| McpError::resource_not_found(error, None)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::run_migrations;
    use std::path::PathBuf;

    fn test_server() -> MemoryServer {
        let conn = Connection::open_in_memory().expect("open in-memory");
        run_migrations(&conn).expect("migrate");
        MemoryServer::new(
            Config {
                data_dir: PathBuf::from("/tmp/agent-memory-test"),
                db_path: PathBuf::from("/tmp/agent-memory-test/memory.db"),
                model_cache_dir: PathBuf::from("/tmp/agent-memory-test/models"),
                gateway: Default::default(),
            },
            conn,
        )
    }

    fn insert_memory(server: &MemoryServer, content: &str, project: Option<&str>) -> String {
        let memory = Memory::new(
            content.to_string(),
            Some(vec!["okf-test".to_string()]),
            project.map(str::to_string),
            Some("test-agent".to_string()),
            None,
            Some("project".to_string()),
        );
        let id = memory.id.clone();
        queries::insert_memory(&server.conn.lock().unwrap(), &memory).expect("insert memory");
        id
    }

    #[test]
    fn working_get_absent_returns_present_false() {
        let server = test_server();
        let out = server.working_get(Parameters(WorkingGetArgs {
            project: Some("agent-memory".to_string()),
        }));
        assert_eq!(
            out,
            r#"<working_context project="agent-memory" present="false"/>"#
        );
    }

    #[test]
    fn working_set_get_clear_round_trip() {
        let server = test_server();
        let set = server.working_set(Parameters(WorkingSetArgs {
            project: Some("agent-memory".to_string()),
            content: "handoff body".to_string(),
        }));
        assert!(set.contains("status=\"working_context_set\""));
        assert!(set.contains("project=\"agent-memory\""));

        let get = server.working_get(Parameters(WorkingGetArgs {
            project: Some("agent-memory".to_string()),
        }));
        assert!(get.contains("present=\"true\""));
        assert!(get.contains("handoff body"));

        let clear = server.working_clear(Parameters(WorkingClearArgs {
            project: Some("agent-memory".to_string()),
        }));
        assert!(clear.contains("status=\"working_context_cleared\""));
        assert!(clear.contains("deleted=\"true\""));

        let absent = server.working_get(Parameters(WorkingGetArgs {
            project: Some("agent-memory".to_string()),
        }));
        assert!(absent.contains("present=\"false\""));
    }

    #[test]
    fn working_set_rejects_global_and_size_cap() {
        let server = test_server();
        let global = server.working_set(Parameters(WorkingSetArgs {
            project: Some(GLOBAL_PROJECT_IDENT.to_string()),
            content: "handoff".to_string(),
        }));
        assert!(global.contains("status=\"error\""));
        assert!(global.contains("per-project only"));

        let too_long = "x".repeat(queries::WORKING_CONTEXT_MAX_CHARS + 1);
        let large = server.working_set(Parameters(WorkingSetArgs {
            project: Some("agent-memory".to_string()),
            content: too_long,
        }));
        assert!(large.contains("status=\"error\""));
        assert!(large.contains("maximum is 65536"));
    }

    #[test]
    fn project_move_transfers_working_context() {
        let server = test_server();
        {
            let conn = server.conn.lock().unwrap();
            let memory = Memory::new(
                "legacy content".to_string(),
                None,
                Some("legacy-project".to_string()),
                None,
                None,
                Some("project".to_string()),
            );
            queries::insert_memory(&conn, &memory).unwrap();
            queries::set_working_context(&conn, "legacy-project", "handoff").unwrap();
        }

        let dry_run = server.move_tool(Parameters(MoveArgs {
            id: None,
            from: Some("legacy-project".to_string()),
            to: "agent-memory".to_string(),
            dry_run: Some(true),
        }));
        assert!(dry_run.contains("status=\"dry_run\""));
        assert!(dry_run.contains("working_context=\"would_move\""));

        let moved = server.move_tool(Parameters(MoveArgs {
            id: None,
            from: Some("legacy-project".to_string()),
            to: "agent-memory".to_string(),
            dry_run: Some(false),
        }));
        assert!(moved.contains("status=\"moved\""));
        assert!(moved.contains("count=\"1\""));
        assert!(moved.contains("working_context=\"moved\""));

        let conn = server.conn.lock().unwrap();
        assert!(queries::get_working_context(&conn, "legacy-project")
            .unwrap()
            .is_none());
        assert_eq!(
            queries::get_working_context(&conn, "agent-memory")
                .unwrap()
                .expect("moved handoff")
                .content,
            "handoff"
        );
    }

    #[test]
    fn okf_tools_cover_validate_get_put_list_history_diff_and_graph() {
        let server = test_server();
        let first = insert_memory(&server, "first body", Some("alpha/project"));
        let second = insert_memory(&server, "second body", Some("alpha/project"));
        {
            let conn = server.conn.lock().unwrap();
            queries::update_content(&conn, &first, "updated body", None, None).unwrap();
            crate::concepts::insert_relationship(
                &conn,
                &first,
                Some(&second),
                &agent_memory::concepts::canonical_uri(&second, Some("alpha/project")),
                "supports",
                2,
                Some(0),
            )
            .unwrap();
        }

        let document = server.okf_get(Parameters(OkfGetArgs {
            target: first.clone(),
        }));
        assert!(document.contains("updated body"));
        assert!(document.starts_with("---\n"));
        assert!(server
            .okf_validate(Parameters(OkfValidateArgs {
                content: document.clone(),
            }))
            .contains("status=\"okf_valid\""));

        let listed = server.okf_list(Parameters(OkfListArgs {
            bundle: agent_memory::okf::BundleScope::Project("alpha/project".into()).uri(),
            path: Some("/memories/".into()),
            limit: Some(10),
        }));
        assert!(listed.contains(&first));
        assert!(listed.contains(&second));

        let history = server.okf_history(Parameters(OkfHistoryArgs {
            id: first.clone(),
            limit: Some(10),
        }));
        assert!(history.contains("revision=\"2\""));
        assert!(history.contains("revision=\"1\""));

        let diff = server.okf_diff(Parameters(OkfDiffArgs {
            id: first.clone(),
            revision_a: 1,
            revision_b: 2,
        }));
        assert!(diff.contains("field=\"body\""));

        let graph = server.okf_graph(Parameters(OkfGraphArgs {
            target: first.clone(),
            relation: Some("supports".into()),
            direction: Some("out".into()),
            depth: Some(2),
            limit: Some(10),
        }));
        assert!(graph.contains("status=\"okf_graph_path\""));
        assert!(graph.contains(&second));

        let dry_run = server.okf_put(Parameters(OkfPutArgs {
            bundle: agent_memory::okf::BundleScope::Project("alpha/project".into()).uri(),
            target: Some(first),
            content: document,
            expected_revision: Some(2),
            dry_run: Some(true),
        }));
        assert!(dry_run.contains("status=\"okf_put_dry_run\""));
    }

    #[test]
    fn okf_resources_are_markdown_scope_isolated_and_exclude_working_context() {
        let server = test_server();
        let id = insert_memory(&server, "resource body", Some("resource-project"));
        queries::set_working_context(
            &server.conn.lock().unwrap(),
            "resource-project",
            "secret handoff marker",
        )
        .unwrap();

        let resources = server.okf_resources().unwrap();
        assert_eq!(resources.len(), 2);
        assert!(resources.iter().all(|resource| {
            resource
                .uri
                .starts_with("okf+memory://project/resource-project/")
                && resource.mime_type.as_deref() == Some("text/markdown")
        }));
        let index = server
            .read_okf_resource("okf+memory://project/resource-project/index.md")
            .unwrap();
        assert!(index.contains(&id));
        assert!(!index.contains("secret handoff marker"));
        let document = server
            .read_okf_resource(&format!(
                "okf+memory://project/resource-project/memories/{id}.md"
            ))
            .unwrap();
        assert!(document.contains("resource body"));
        let other = server
            .read_okf_resource("okf+memory://project/other/index.md")
            .unwrap();
        assert!(!other.contains(&id));
        assert!(!other.contains("resource body"));
        assert!(server
            .read_okf_resource("okf+memory://project/resource-project/working-context.md")
            .is_err());
    }

    #[test]
    fn all_okf_tool_argument_schemas_serialize() {
        let schemas = [
            serde_json::to_value(schemars::schema_for!(OkfValidateArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(OkfGetArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(OkfPutArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(OkfListArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(OkfHistoryArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(OkfDiffArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(OkfGraphArgs)).unwrap(),
        ];
        assert!(schemas.iter().all(serde_json::Value::is_object));
    }
}
