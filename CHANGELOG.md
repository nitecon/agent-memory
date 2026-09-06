# Changelog

## v1.13.3 - 2026-09-06

- Fixed `memory hook` waiting indefinitely for terminal input. Terminal and
  empty-stdin calls now return the current project's WorkingContext directly.
- Included the latest WorkingContext and all appended amendments in agent
  prompt hook results, with a WorkingContext fallback if memory retrieval fails.
- Added a trailing hint to use `memory working append` for follow-up notes,
  decisions, and next steps for future agents continuing the current task.

## v1.13.2 - 2026-09-04

- Added `memory working append`, which preserves the current WorkingContext and
  adds a blank-line-delimited `## Ammendment N` section. Amendment numbering is
  automatic, and inline, stdin, and file input are supported.

## v1.13.1 - 2026-09-01

- Added `{prompt_stdin}` transport for headless Dream adapters. The marker is
  removed from argv and the complete prompt is streamed verbatim to the child,
  so project-review prompts are no longer bounded by Linux `MAX_ARG_STRLEN`.
- Made inference degradation fail visibly. Project-review inference and parse
  fallbacks now emit `review_failed` and increment the pass failure count, and
  the Stage B no-op contract now requires `KEEP_UNCHANGED`; the pathological
  legacy `skip` response is rejected instead of recording a false success.
  Review JSON must also cover exactly the real batch IDs; missing or invented
  keys reject the response rather than silently defaulting to keep.
- Made gateway tombstones idempotent when a rejected response explicitly says
  the linked gateway memory is already absent. Dream can then complete the
  local removal while unrelated gateway conflicts remain fail-closed.

## v1.13.0 - 2026-08-16

- Applied one OKF lifecycle predicate to every retrieval surface. Graph
  expansion traverses both directions, so a live memory's `supersedes` edge
  reached the predecessor Dream had merged away and recall injected both the
  survivor and the row it replaced; `memory get` then refused the advertised
  short id because prefix resolution hides superseded rows. Superseded and
  deprecated concepts are now excluded before ranking, and a prefix that
  matches only an audit row resolves instead of reporting not found.
- Derived OKF descriptors structurally on write and on use. `title`,
  `description` and `sources` no longer wait on a scheduled Dream pass: they
  are derived from the body's own structure, fill only absent fields, record
  what was inferred under `x-agent-memory-derived`, and decline rather than
  invent a title when the body has no headline shape. Added `memory enrich`
  to sweep an existing store. Concepts written from OKF documents are left
  alone, since the document is authoritative.
- Carried the OKF concept across Dream replacements. A supersede or extract
  previously restarted the concept at revision 1, discarding domain type,
  descriptors, lifecycle, sources and round-tripped unknown frontmatter.
  Provenance edges now address the deleted predecessor by canonical URI, and
  the relationship producer is derived from the acting actor rather than
  hardcoded.
- Attributed curated bodies. Dream stamps OKF `generated` with the
  backend-qualified model identity when it rewrites a body, so a
  model-authored memory no longer credits whoever stored the original text.
- Namespaced curation revision operations and relabelled existing history, so
  an audit filter written against the documented vocabulary sees every
  revision Dream wrote.
- Persisted contradictions as `contradicts` edges carrying their reason
  instead of only printing them, and recorded a machine confirmation when a
  pass re-reads a memory and leaves it standing. Confirming is a no-op once
  the current body already carries that curator's attestation.
- Added `deprecate` as a curation decision between keep and drop, retiring a
  memory that has stopped being the live answer while keeping its body and
  history.

## v1.12.2 - 2026-08-16

- Built Linux release artifacts inside a fixed Debian 12 environment and
  rejected binaries requiring newer than GLIBC 2.36, restoring compatibility
  with Debian 12 and preventing moving GitHub runners from silently raising
  the runtime ABI floor.
- Documented the shell-installer recovery path for hosts where an incompatible
  binary cannot launch its own updater, and made that installer execute-check
  both staged binaries before replacing either installed executable.

## v1.12.1 - 2026-08-15

- Made Dream's condenser audit stamp use the actual backend-qualified
  inference identity. Headless configuration now has an explicit model field
  and `{model}` command placeholder; legacy pinned commands migrate without
  overwriting custom templates.
- Isolated headless Dream subprocesses from automatic memory/agent-tools hook
  injection, corrected Dream-created relationship producer provenance, and
  repair deterministically attributable legacy relationship rows on migration.
- Added a cross-platform single-pass lock, made `--limit` a true global cap,
  included inference identity and elapsed time in terminal summaries, and made
  the binary exit non-zero when any Dream operations fail.
- Kept scheduled Dream passes unlimited by default so every eligible memory is
  considered; pass limits remain an explicit operator opt-in.

## v1.12.0 - 2026-08-15

- Made every durable SQLite memory a canonical OKF concept with lossless virtual
  Markdown documents/bundles, generated indexes/logs, immutable semantic
  revisions, compare-and-swap updates, provenance, verification, lifecycle
  state, arbitrary extensions, and bounded typed graph traversal.
- Added `memory okf` validate/get/put/read/list/index/log/history/diff/graph,
  explicit safe import/export, corresponding MCP tools, and
  `okf+memory://...` Markdown resources. WorkingContext remains completely
  outside the OKF surface.
- Extended retrieval and Dream curation with OKF metadata, freshness/trust,
  graph context, provenance-aware revisions, verification invalidation, and
  contradiction preservation.
- Added an explicitly capability-gated gateway `okf-markdown` envelope and
  semantic hash while retaining legacy fields, hashes, conflict behavior, and
  tombstones. Unknown extension fields round-trip across mixed-version sync.
- Made the complete schema upgrade chain atomic and added migration retry,
  WAL concurrency, hostile-input, graph-budget, and release injection-probe
  conformance coverage.

## v1.11.1 - 2026-06-18

- Gateway-aware rules block now decides **programmatically at runtime** from gateway-configured state: when a gateway is configured the injected block omits the post-action save directive (Rule B + quality gate, because the gateway's `tasks done` reminder owns the save nudge), otherwise it keeps Rule B as the fallback. Removed the `AGENT_MEMORY_GATEWAY_SAVE_REMINDER` env var / config flag introduced in v1.11.0 — derivable state should not require a manually-set environment variable.

## v1.11.0 - 2026-06-18

- Added `memory setup hooks` (opt-in POC): wires automatic per-turn RAG memory injection into each agent CLI's hook system so relevant memory is injected into context every turn WITHOUT the agent calling `memory context` itself. Installs a shared bridge script at `~/.agentic/hooks/memory-inject.sh` and registers it per agent — Claude Code (`UserPromptSubmit`), Codex (`UserPromptSubmit`), Gemini CLI (`BeforeAgent`). Merges are conservative, idempotent, and atomic; `--remove` strips only our marker-matching entries, collapses emptied parents, and deletes the script. Detection reuses the same agent resolution as `memory setup rules`. This component is **not** part of `memory setup all` — it is selectable only via the explicit subcommand or the interactive checklist's 4th option.
- Trimmed the injected `<memory-rules>` block: removed the mandatory "Rule A — Pre-action behavior recall (run `memory context` first)" section, replaced by a terse note that recall is now automatic via the memory hook. Post-action scope-classification (Rule B), Operations, WorkingContext, and the memory quality gate are unchanged — saving stays rule-driven for now.
- Added a `memory context --no-working-context` flag that omits the WorkingContext block (and its absence hint) from the output. The per-turn injection hook uses it so WorkingContext — already injected once by the session-start hook and refreshed across compaction — is not re-emitted every turn.
- Made the injected `<memory-rules>` block gateway-aware: when the agent-tools gateway is configured **and** the new `AGENT_MEMORY_GATEWAY_SAVE_REMINDER` cutover flag is enabled, the block now omits the post-action save directive (Rule B scope-classification + memory quality gate) because the gateway's `tasks done` reminder delivers the save nudge instead. The flag **defaults off** (accepts the `MEMORY_GATEWAY_SAVE_REMINDER` alias, same config-file + env precedence as `AGENT_MEMORY_GATEWAY_AUTO_SYNC`), so every current install — and any install without a configured gateway — keeps Rule B as the fallback save rule and no save gap is created until the gateway reminder ships.

## v1.10.0 - 2026-06-10

- Added all-scope gateway sync: `memory push --all` / `memory pull --all` exchange every local/remote durable-memory project ident (including `__global__`) in one pass, while default (scoped) push/pull continue to operate on the current project ident only. WorkingContext never participates in gateway exchange.

## v1.9.0 - 2026-06-09

- Added a local cross-encoder rerank stage to hybrid search: BM25 (FTS5) + cosine results fused via RRF are now re-ordered by a local fastembed cross-encoder (on by default) for sharper top-K relevance. Documented in the README architecture section.

## v1.8.7 - 2026-06-05

- Fixed gateway project lookup for remotes whose repository name differs by case from the gateway ident, e.g. `git@github.com:nitecon/X.git` now resolves to `x` for `memory pull`/`memory push`.
- Lowercased the non-git directory fallback ident to avoid Windows/PowerShell path-case drift creating a separate project id.

## v1.8.6 - 2026-06-05

- Fixed cwd project detection on Windows/worktree-style checkouts by reading `.git/config` directly, including the current branch's upstream remote, before falling back to the directory basename. This avoids gateway sync using an uppercase directory name when the repository ident is lowercase.

## v1.8.5 - 2026-06-05

- Added best-effort gateway auto-sync after `memory store` for project-scoped memories: when gateway URL/API key are configured and auto-sync is enabled, the CLI pushes pending local project memories and then pulls remote project memories.
- Added persisted `AGENT_MEMORY_GATEWAY_AUTO_SYNC` / `MEMORY_GATEWAY_AUTO_SYNC` config parsing and a `memory setup gateway` prompt to disable the default-on behavior.

## v1.8.3 - 2026-05-29

- `memory pull` now accepts boolean `tombstone` flags in gateway responses: `true` is treated as a deletion (synthesizing a tombstone), while `false`/`null`/absent mean "live"; object-form tombstones continue to deserialize as before.

## v1.8.2 - 2026-05-29

- Gateway push now serializes `created_at`/`updated_at` timestamps as epoch-millis integers to match the gateway wire format (previously RFC3339 strings were sent, which the gateway rejected).

## v1.8.1 - 2026-05-29

- Added `memory setup gateway` to configure the shared agent-gateway connection. It writes `~/.agentic/agent-tools/gateway.conf`, the same file `agent-tools setup gateway` uses.
- `memory setup all` now runs components in the order gateway → rules → skill (was rules → skill), so the URL/key that `memory push`/`memory pull` depend on are configured first.
- Gateway URL/key now resolve from shared config files — `/opt/agentic/agent-tools/gateway.conf` (system) and `~/.agentic/agent-tools/gateway.conf` (user) — in addition to environment variables. Added `GATEWAY_URL`/`GATEWAY_API_KEY` env (and config-file) fallbacks alongside the existing `AGENT_MEMORY_GATEWAY_*` and `AGENT_GATEWAY_*` names.
- Aligned the gateway push/pull wire format with the gateway: renamed request/response fields (e.g. `since_revision`, `page_size`, `project_ident`, `base_gateway_revision` — old names retained as back-compat aliases), tombstone records in pull responses, and epoch-millis timestamp deserialization.
- **Breaking for local sync state:** `content_hash` is now the plain lowercase SHA-256 hex of the memory content only, with no `sha256:` prefix and no longer mixing in type/tags. This invalidates previously stored local sync hashes, so the first `memory push`/`memory pull` after upgrading re-evaluates every project memory.
- Added the `rpassword` dependency for masked gateway-key entry during setup.

## v1.8.0 - 2026-05-29

- Added project memory gateway exchange: `memory push` and `memory pull` (each with a read-only `status` subcommand) sync durable memories for the current project ident with an agent gateway. Global (`__global__`) memories and WorkingContext are excluded.
  - Push outcomes are `created`, `updated`, `linked`, `conflict`, or `rejected`; pull outcomes are `import`, `update`, `link`, `tombstone`, `conflict`, `skipped`, or `rejected`. Conflicts are reported without overwriting local content, and semantic near-duplicates are not silently merged on pull.
  - Gateway URL/key are read from `AGENT_MEMORY_GATEWAY_URL`/`AGENT_MEMORY_GATEWAY_API_KEY`, falling back to the generic `AGENT_GATEWAY_URL`/`AGENT_GATEWAY_API_KEY`.
- Changed the default database path: fresh installs now store data under the user-writable `~/.agentic/` directory so first-run model downloads and DB writes do not depend on `/opt` permissions. Existing installs with a writable `/opt/agentic/memory.db` continue to use it as a legacy shared database; `AGENT_MEMORY_DIR` still overrides explicitly.

## v1.7.0 - 2026-05-18

- Added per-project WorkingContext handoff state with `memory working get`, `memory working set`, and `memory working clear`.
- `memory context` and `memory_context` now render active WorkingContext before ranked memories without consuming the search result budget.
- Added MCP tools `memory_working_get`, `memory_working_set`, and `memory_working_clear`.
- Added `.agent/api/memory.yaml` API context documentation for the memory CLI and MCP surfaces.
- Project-wide `memory move` and `memory_move` now transfer or clear WorkingContext; `memory copy` and `memory projects` remain durable-memory-only.
