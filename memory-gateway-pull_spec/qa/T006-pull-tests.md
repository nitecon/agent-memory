# T006 - Add pull tests and conflict coverage

**Team:** qa
**Phase:** 3
**Depends on:** T005
**Status:** todo

## Scope

**In:** Add focused tests for pull candidate handling, imports,
fast-forwards, tombstones, status read-only behavior, and conflicts.

**Out:** Live gateway integration tests unless a stable test gateway fixture is
available.

## Source references

- `memory-gateway-exchange.md`
- `memory-gateway-pull_spec/backend/T005-pull-cli.md`

## Deliverables

1. **Contract tests** for pull request/response fixtures.
2. **DB tests** for cursor, imported mapping, and tombstone metadata.
3. **CLI tests** for status and mutation rendering.

## Implementation Notes

- Prefer deterministic fixtures over live network calls.
- Include explicit negative coverage for global memories and WorkingContext.
- Confirm status mode does not update cursor state.

## Acceptance Criteria

- [ ] Tests prove global memories and WorkingContext are never imported through pull.
- [ ] Tests prove new remote memories import under the current project ident.
- [ ] Tests prove remote updates fast-forward only when the local base hash still matches.
- [ ] Tests prove status mode performs no local content or metadata writes.
- [ ] `cargo test -p memory` passes.

## Validation Plan

- Run `cargo test -p memory`.
- Run pull-specific tests individually during debugging.

## Dependencies

- T005 implemented CLI behavior.

## Provides To Downstream Tasks

- Confidence for enabling pull against the delegated gateway API.
