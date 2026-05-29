# T001 - Define pull contract structs and fixtures

**Team:** backend
**Phase:** 1
**Depends on:** (none)
**Status:** todo

## Scope

**In:** Add Rust data models for the gateway pull request and response. The
request carries the current project ident plus cursor/revision state. The
response returns an array of project-only memory structs plus cursor and
revision metadata.

**Out:** HTTP transport, DB import behavior, and CLI command parsing.

## Source references

- `memory-gateway-exchange.md`
- Delegated gateway task `019e73fd-8860-7781-b3a6-878af49b8c36`

## Deliverables

1. **`memory/src/sync.rs`** - pull request, response, remote memory struct,
   tombstone, provenance, and action types.
2. **`memory/src/lib.rs`** - exports the sync module if needed by tests.
3. **`memory/tests/gateway_pull_contract.rs`** - fixture tests for response
   shape and malformed-project rejection.

## Implementation Notes

- Keep the pull contract compatible with the push memory struct where practical.
- Include provenance metadata so imported memories can render or debug their
  source without affecting normal retrieval ranking.
- Treat WorkingContext as out of scope.

## Acceptance Criteria

- [ ] Rust request/response structs represent the gateway pull API as project ident plus cursor/revision state returning an array of project-only memory structs.
- [ ] Structs carry gateway memory ID, server revision, content, memory type, tags, project ident, content hash, provenance, and tombstone status.
- [ ] Structs exclude global memories and WorkingContext by construction or by explicit validation.
- [ ] Serialization fixtures cover new remote memory, remote update, tombstone, paginated cursor, and malformed-project rejection cases.
- [ ] The spec references delegated gateway pull API task `019e73fd-8860-7781-b3a6-878af49b8c36` as the external contract owner.

## Validation Plan

- Run `cargo test -p memory gateway_pull_contract`.
- Confirm the top-level response shape contains `memories` and a next
  revision/cursor.
- Confirm no field carries global/user-preference state.

## Dependencies

None.

## Provides To Downstream Tasks

- **T002:** local metadata stores gateway IDs, revisions, and cursor state.
- **T003:** gateway client uses these models as its transport boundary.
