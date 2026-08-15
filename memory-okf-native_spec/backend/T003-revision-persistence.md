# T003 - Route semantic memory writes through revision-aware persistence

**Depends on:** T001, T002

## Scope

Create one transactional concept service used by store, update, move, copy,
forget, gateway pull, and later Dream/OKF put. It must create immutable semantic
revisions, support compare-and-swap, maintain normalized side tables, create
audit tombstones, and keep transactions short.

## Acceptance criteria

- [ ] All existing semantic writers use the service or a proven equivalent transactional primitive.
- [ ] Semantic no-op updates reuse the current hash and do not create duplicate revisions.
- [ ] Stale expected revisions conflict without changing any table.
- [ ] Meaningful changes clear current verification unless replacements are supplied atomically.
- [ ] Copy and extract create `derived_from`; move records URI alias; supersession records a typed edge.
- [ ] Forget records an audit tombstone before cascading live concept state.
- [ ] Existing gateway direct-write and retry semantics remain intact.
- [ ] No write transaction spans embedding, inference, filesystem, or network calls.
- [ ] Existing CLI/MCP behavior and tests remain compatible.

## Touch surface

- `memory/src/concepts/`
- `memory/src/db/queries.rs`
- `memory/src/cli.rs`
- `memory/src/mcp.rs`
- `memory/src/gateway_sync.rs`

## Validation

- targeted store/update/move/copy/forget/pull tests
- concurrent read/write and rollback tests
