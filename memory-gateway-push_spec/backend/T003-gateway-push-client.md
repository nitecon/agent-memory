# T003 - Implement gateway push client

**Team:** backend
**Phase:** 2
**Depends on:** T001, T002
**Status:** todo

## Scope

**In:** Add an HTTP client boundary that sends project-only memory push batches
to the gateway and classifies responses.

**Out:** CLI parsing, rendering, and conflict resolution commands.

## Source references

- `memory-gateway-exchange.md`
- Delegated gateway task `019e73fd-8869-7451-8c75-4b7c7a96b4f8`

## Deliverables

1. **Gateway client function** - sends push batches and returns typed per-item
   outcomes.
2. **Configuration lookup** - reads gateway base URL and token from the existing
   configuration surface or a narrow new config entry.
3. **Response classifier** - converts gateway responses into local actions.

## Implementation Notes

- Keep request construction separate from transport so unit tests can validate
  candidate filtering without network access.
- Use compare-and-swap semantics: include base server revision when known.
- Treat transient HTTP failures as retryable and validation/auth failures as
  non-retryable.

## Acceptance Criteria

- [ ] Client sends one batch containing the cwd-derived project ident and project-only memory structs.
- [ ] Client includes gateway IDs and base revisions for previously linked records.
- [ ] Client treats exact content-hash links, created records, updated records, conflicts, and rejected records as separate outcomes.
- [ ] HTTP error handling distinguishes authentication, project authorization, validation, transient gateway failure, and malformed response.
- [ ] Unit tests cover request construction and response classification without requiring a live gateway.

## Validation Plan

- Run `cargo test -p memory gateway_push`.
- Use a mock transport or pure response parser tests for each gateway action.
- Confirm no network call is made by status-only tests.

## Dependencies

- T001 request/response models.
- T002 sync metadata.

## Provides To Downstream Tasks

- **T004:** CLI commands call this client for mutation mode.
- **T005:** tests exercise response classification and conflict handling.
