# T001 - Define push contract structs and fixtures

**Team:** backend
**Phase:** 1
**Depends on:** (none)
**Status:** todo

## Scope

**In:** Add Rust data models for the gateway push request and response. The
request is one project ident plus an array of project-only memory structs. The
response is per-item and records canonical gateway identity, revision, action,
conflicts, and validation failures.

**Out:** HTTP transport, CLI command parsing, DB migrations, and actual push
execution.

## Source references

- `memory-gateway-exchange.md`
- Delegated gateway task `019e73fd-8869-7451-8c75-4b7c7a96b4f8`

## Deliverables

1. **`memory/src/sync.rs`** - new module containing serializable push request,
   memory struct, response, action, conflict, and validation-error types.
2. **`memory/src/lib.rs`** - exports the sync module if needed by tests.
3. **`memory/tests/gateway_push_contract.rs`** - fixture tests for request and
   response JSON.

## Implementation Notes

- Keep the gateway model separate from DB row structs. The gateway contract is
  an API boundary and should not leak SQLite implementation details.
- Use explicit fields for `project`, `memory_type`, `tags`, `content_hash`,
  optional `gateway_memory_id`, and optional `base_server_revision`.
- Treat global-scope memories and WorkingContext as invalid push inputs.

## Acceptance Criteria

- [ ] Rust request/response structs represent the gateway push API as project ident plus an array of project-only memory structs.
- [ ] Structs exclude global memories and WorkingContext by construction or by explicit validation.
- [ ] Per-item response model includes gateway memory ID, server revision, action, conflict metadata, and validation errors.
- [ ] Serialization fixtures cover create, update, exact-hash link, conflict, and rejected-secret cases.
- [ ] The spec references delegated gateway push API task `019e73fd-8869-7451-8c75-4b7c7a96b4f8` as the external contract owner.

## Validation Plan

- Run `cargo test -p memory gateway_push_contract`.
- Inspect the serialized fixture to confirm the top-level shape is
  `{ project, memories: [...] }`.
- Confirm there is no field that can carry global/user-preference scope.

## Dependencies

None.

## Provides To Downstream Tasks

- **T002:** sync metadata stores gateway IDs and revisions returned by these
  response structs.
- **T003:** gateway client uses these models as its transport boundary.
