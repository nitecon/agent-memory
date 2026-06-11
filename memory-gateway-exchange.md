# Project Memory Gateway Exchange

## Goal

Share durable project and global memories between machines through the agent
gateway so learning discovered by one system can be reused by other systems,
even when the same repositories do not exist on every machine.

## Constraints

- Default `memory push` / `memory pull` sync only the current project ident.
- `memory push --all` syncs every local durable-memory project ident,
  including `__global__`.
- `memory pull --all` discovers remote memory project idents from the gateway
  before pulling, including projects absent locally and `__global__` when
  present.
- Exclude WorkingContext; it remains a separate transient handoff surface.
- Use explicit directional commands rather than a generic sync command:
  `memory push status`, `memory push`, `memory pull status`, `memory pull`,
  and their `--all` variants.
- Gateway APIs are array-oriented over memory structs.
- Host, OS, architecture, and agent-origin metadata belongs in structured
  `GatewayMemory.provenance`, not normal memory tags.
- Deconfliction is based on gateway memory IDs, gateway revisions, and content
  hashes.
- Exact content-hash duplicates can be linked automatically.
- Semantic near-duplicates are reported for later consolidation, not silently
  merged during push or pull.

## Gateway API Shape

`memory push` sends a project ident and an array of memory structs for that
project. The gateway returns per-item actions such as created, updated, linked,
conflict, or rejected, plus canonical gateway IDs and server revisions.
`memory push --all` repeats that operation for every local durable-memory
project ident, including `__global__`.

`memory pull` sends a project ident and cursor/revision state. The gateway
returns an array of memory structs representing new or changed remote records,
plus cursor, revision, provenance, and tombstone metadata. `memory pull --all`
first calls the gateway project-discovery route, then repeats the per-project
pull for each returned ident.

## Local Client Responsibilities

- Keep local memory retrieval behavior unchanged. `memory store` may now run
  best-effort project sync automatically when gateway auto-sync is enabled:
  push pending local project memories first, then pull remote project memories.
  Manual `memory push`, `memory pull`, and their `--all` variants remain the
  explicit diagnostic and retry surfaces.
- Add sync metadata that maps local memory IDs to gateway memory IDs and server
  revisions.
- Detect push and pull conflicts without silently overwriting local or remote
  edits.
- Provide status commands that show exactly what would move before mutation.
- Preserve project-ident behavior already used by `memory context`, `store`,
  `search`, and `recall`.

## Gateway Dependencies

The gateway-side API work is tracked by delegated tasks:

- Push API: `019e73fd-8869-7451-8c75-4b7c7a96b4f8`
- Pull API: `019e73fd-8860-7781-b3a6-878af49b8c36`
