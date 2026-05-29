# T005 - Add push tests and conflict coverage

**Team:** qa
**Phase:** 3
**Depends on:** T004
**Status:** todo

## Scope

**In:** Add focused tests for candidate selection, idempotency, status
read-only behavior, and conflict handling.

**Out:** Live gateway integration tests unless a stable test gateway fixture is
available.

## Source references

- `memory-gateway-exchange.md`
- `memory-gateway-push_spec/backend/T004-push-cli.md`

## Deliverables

1. **Contract tests** for push request/response fixtures.
2. **DB tests** for metadata behavior.
3. **CLI tests** for status and mutation rendering.

## Implementation Notes

- Prefer mock client tests over live gateway dependency.
- Include explicit negative coverage for global memories and WorkingContext.
- Confirm conflicts leave local sync metadata untouched.

## Acceptance Criteria

- [ ] Tests prove global memories and WorkingContext are never included in push candidates.
- [ ] Tests prove exact content-hash duplicates are linked rather than re-created.
- [ ] Tests prove moved remote base revision creates a conflict and leaves local metadata unchanged.
- [ ] Tests prove status mode performs no local sync metadata writes.
- [ ] `cargo test -p memory` passes.

## Validation Plan

- Run `cargo test -p memory`.
- Run the specific new push test names individually during debugging.

## Dependencies

- T004 implemented CLI behavior.

## Provides To Downstream Tasks

- Confidence for enabling the command against the delegated gateway API.
