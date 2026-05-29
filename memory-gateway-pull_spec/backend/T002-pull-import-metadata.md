# T002 - Add pull cursor and import metadata

**Team:** backend
**Phase:** 1
**Depends on:** T001
**Status:** todo

## Scope

**In:** Add local metadata needed to track project pull progress and imported
gateway records.

**Out:** Network calls, import mutation logic, and CLI rendering.

## Source references

- `memory-gateway-exchange.md`
- `memory/src/db/queries.rs`

## Deliverables

1. **Project pull cursor state** - last seen gateway revision or cursor for the
   current project ident.
2. **Imported memory mapping** - local memory ID to gateway memory ID/revision
   mapping, shared with push metadata where practical.
3. **Tombstone metadata** - enough state to report remote deletes without
   hard-deleting local content.

## Implementation Notes

- Reuse the push sync metadata table if T002 from the push spec has already
  landed; otherwise design the table for both push and pull.
- Do not bump local memory `updated_at` when only cursor or mapping metadata
  changes.
- Project ident must be part of cursor and mapping validation.

## Acceptance Criteria

- [ ] SQLite metadata records last pulled server revision or cursor for the current project ident.
- [ ] Local memory to gateway memory mapping can be created for pulled remote records.
- [ ] Metadata supports tombstone tracking without hard-deleting local memories automatically.
- [ ] Tests cover fresh DB migration, cursor round trip, imported mapping round trip, and project-ident mismatch rejection.
- [ ] Metadata updates do not modify memory content timestamps unless a memory body is actually imported or updated.

## Validation Plan

- Run `cargo test -p memory db::`.
- Add cursor round-trip tests for two different project idents.
- Add a tombstone metadata test that leaves memory content intact.

## Dependencies

- T001 pull contract structs.

## Provides To Downstream Tasks

- **T003:** supplies cursor state for gateway requests.
- **T004:** stores import, fast-forward, and tombstone outcomes.
