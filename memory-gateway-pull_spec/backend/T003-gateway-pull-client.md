# T003 - Implement gateway pull client

**Team:** backend
**Phase:** 2
**Depends on:** T001, T002
**Status:** todo

## Scope

**In:** Add an HTTP client boundary that fetches project-only memory diffs from
the gateway.

**Out:** Local import mutation logic and CLI parsing.

## Source references

- `memory-gateway-exchange.md`
- Delegated gateway task `019e73fd-8860-7781-b3a6-878af49b8c36`

## Deliverables

1. **Gateway client function** - requests diffs with project ident and cursor.
2. **Pagination handling** - follows stable next-cursor or next-revision data.
3. **Response classifier** - categorizes remote records before local mutation.

## Implementation Notes

- Keep transport independent from import so status mode can preview actions.
- Make page limits explicit to avoid unbounded command runtime.
- Treat malformed project identifiers from the gateway as hard validation
  errors.

## Acceptance Criteria

- [ ] Client requests diffs using current project ident and the local last-seen gateway revision or cursor.
- [ ] Client handles stable pagination until complete or until a configured page limit is reached.
- [ ] Client classifies new remote records, remote updates, tombstones, exact-hash links, conflicts, and rejected records.
- [ ] HTTP error handling distinguishes authentication, project authorization, validation, transient gateway failure, and malformed response.
- [ ] Unit tests cover request construction, pagination, and response classification without requiring a live gateway.

## Validation Plan

- Run `cargo test -p memory gateway_pull`.
- Test a two-page fixture and confirm output order is stable.
- Test transient gateway errors without mutating local state.

## Dependencies

- T001 request/response models.
- T002 cursor metadata.

## Provides To Downstream Tasks

- **T004:** remote action set for import.
- **T005:** CLI status and mutation behavior.
