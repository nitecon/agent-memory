# T002 - Add local gateway sync metadata

**Team:** backend
**Phase:** 1
**Depends on:** T001
**Status:** todo

## Scope

**In:** Add local SQLite metadata for mapping local memory records to gateway
memory records and revisions.

**Out:** Network calls and CLI command behavior.

## Source references

- `memory-gateway-exchange.md`
- `memory/src/db/queries.rs`

## Deliverables

1. **SQLite migration** - table or columns that track local memory ID, gateway
   memory ID, last seen server revision, last pushed content hash, sync state,
   project ident, and timestamps.
2. **DB model helpers** - typed read/upsert helpers for sync metadata.
3. **DB tests** - migration and round-trip coverage.

## Implementation Notes

- Prefer a side table keyed by local memory ID so existing durable memory rows
  remain backward compatible.
- Store project ident in the sync metadata and validate it when linking records.
- Do not update memory `updated_at` merely because sync metadata changed.

## Acceptance Criteria

- [ ] SQLite schema records local memory ID, gateway memory ID, last seen server revision, last pushed content hash, sync state, and timestamps.
- [ ] Migration is backward compatible for existing memory databases.
- [ ] DB query helpers can read, upsert, and clear sync metadata for a memory without changing memory content.
- [ ] Tests cover fresh DB migration, existing DB migration, and sync metadata round trip.
- [ ] Metadata is project-aware and cannot link a local memory to a gateway record from a different project ident.

## Validation Plan

- Run `cargo test -p memory db::`.
- Add a test that updates sync metadata and asserts the memory row content and
  `updated_at` are unchanged.
- Add a negative test for project-ident mismatch.

## Dependencies

- T001 gateway response models define the IDs and revisions this metadata
  stores.

## Provides To Downstream Tasks

- **T003:** supplies base revisions and gateway IDs for push requests.
- **T004:** records successful push outcomes.
