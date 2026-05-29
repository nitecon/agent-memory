# T004 - Implement pull import and conflict handling

**Team:** backend
**Phase:** 2
**Depends on:** T003
**Status:** todo

## Scope

**In:** Apply non-conflicting remote pull results into the local memory DB and
record conflicts without overwriting local edits.

**Out:** CLI command parsing and documentation.

## Source references

- `memory-gateway-exchange.md`
- `memory/src/db/queries.rs`

## Deliverables

1. **Import planner** - compares remote records to local memory content,
   content hashes, gateway IDs, and base revisions.
2. **Mutation executor** - applies imports, fast-forwards, links exact
   duplicates, and records tombstones.
3. **Conflict records** - represent local-changed/remote-changed cases without
   destructive writes.

## Implementation Notes

- Fast-forward only when the local content hash matches the last recorded base
  hash/revision.
- Exact content hash duplicates should link to the gateway record instead of
  inserting another memory.
- Semantic near-duplicates are a reporting/consolidation concern for
  `memory-dream`, not an automatic pull merge.

## Acceptance Criteria

- [ ] New remote memories import as local project-scoped durable memories with gateway provenance metadata.
- [ ] Remote updates fast-forward local memories only when the local body has not changed since the recorded base revision/hash.
- [ ] Local edits plus newer remote revision produce a conflict and leave local content unchanged.
- [ ] Tombstones are recorded and reported without automatically hard-deleting local memory content.
- [ ] Exact content-hash duplicates are linked to the gateway record without creating duplicate local memories.

## Validation Plan

- Run `cargo test -p memory gateway_pull`.
- Add table-driven tests for import, fast-forward, conflict, tombstone, and
  exact-hash link.
- Verify local `updated_at` changes only for imported/fast-forwarded memory
  content changes.

## Dependencies

- T003 gateway client and action classification.

## Provides To Downstream Tasks

- **T005:** CLI mutation and status output.
- **T006:** behavior to validate end to end.
