# Changelog

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
