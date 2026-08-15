# T012 - Document and publish the OKF-native memory contract

**Depends on:** T006, T007, T008, T009, T010

## Scope

Document the conceptual model, virtual bundle, CLI/MCP, revisions, graph,
Dream, gateway compatibility, security, recovery, and physical interchange.
Update and publish gateway-backed Documentation context.

## Acceptance criteria

- [ ] README clearly states that SQLite memories are canonical concepts and Markdown is a projection.
- [ ] CLI examples cover get/put, virtual index/log, history/diff/graph, and import/export.
- [ ] Docs distinguish `memory_type` from arbitrary OKF `type`.
- [ ] Docs explain revision/CAS behavior, trust/freshness signals, Dream verification invalidation, and WorkingContext exclusion.
- [ ] Gateway capability and legacy behavior are documented accurately.
- [ ] Recovery guidance covers migration failure, stale derived indexes, conflicts, and export safety.
- [ ] `.agent/api/memory.yaml` matches implemented CLI/MCP/contracts.
- [ ] `agent-tools docs validate` and `agent-tools docs publish` succeed and the publish step is recorded.

## Touch surface

- `README.md`
- `memory/src/cli.rs`
- `memory/src/mcp.rs`
- `.agent/api/memory.yaml`
- `CHANGELOG.md`
