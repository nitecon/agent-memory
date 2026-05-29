# T005 - Implement memory pull status and memory pull

**Team:** backend
**Phase:** 2
**Depends on:** T004
**Status:** todo

## Scope

**In:** Add `memory pull status` and `memory pull` command behavior.

**Out:** Push commands and semantic consolidation.

## Source references

- `memory-gateway-exchange.md`
- `memory/src/cli.rs`
- `memory/src/render/mod.rs`

## Deliverables

1. **CLI subcommand** - `memory pull status` and `memory pull`.
2. **Status renderer** - shows pending remote actions without mutation.
3. **Mutation command** - imports or updates non-conflicting remote memories and
   records local metadata.

## Implementation Notes

- Status mode may need to contact the gateway to know pending remote changes,
  but it must not mutate local content or metadata.
- Output should make conflicts obvious and compact.
- Use existing current-project derivation.

## Acceptance Criteria

- [ ] `memory pull status` fetches or computes pending remote actions and prints them without importing or updating local memories.
- [ ] `memory pull` imports non-conflicting remote diffs for the current project ident.
- [ ] Successful pull records gateway IDs, server revisions, content hashes, provenance, and project cursor locally.
- [ ] Conflicts are reported and do not overwrite local content.
- [ ] Output stays compact and consistent with existing XML-like CLI result style.

## Validation Plan

- Run `cargo test -p memory cli::`.
- Add a status test that uses a mock pull client and verifies no local writes.
- Add a mutation test for imported, linked, tombstone, and conflict outcomes.

## Dependencies

- T004 import planner and executor.

## Provides To Downstream Tasks

- **T006:** end-to-end pull behavior to validate.
- **T007:** documented CLI behavior.
