# T004 - Implement memory push status and memory push

**Team:** backend
**Phase:** 2
**Depends on:** T003
**Status:** todo

## Scope

**In:** Add `memory push status` and `memory push` command behavior.

**Out:** Pull commands and semantic consolidation.

## Source references

- `memory-gateway-exchange.md`
- `memory/src/cli.rs`
- `memory/src/render/mod.rs`

## Deliverables

1. **CLI subcommand** - `memory push status` and `memory push`.
2. **Candidate selection** - project-only durable memories for the current
   project ident.
3. **Renderer** - compact output showing counts and per-record results.
4. **Metadata writes** - only mutation mode records successful gateway outcomes.

## Implementation Notes

- Keep status mode read-only.
- Use existing project-ident derivation rather than adding a separate project
  flag for the first version.
- Follow existing XML-like output style so agent parsing remains cheap.

## Acceptance Criteria

- [ ] `memory push status` prints counts and per-record actions that would be sent without mutating gateway or local sync metadata.
- [ ] `memory push` uploads only project-scoped durable memories for the current project ident.
- [ ] Successful push records gateway IDs, server revisions, content hashes, and sync state locally.
- [ ] Conflicts are reported and do not overwrite local or remote content.
- [ ] Output stays compact and consistent with existing XML-like CLI result style.

## Validation Plan

- Run `cargo test -p memory cli::`.
- Add a status test that snapshots output and then verifies sync metadata is
  unchanged.
- Add a mutation test with a mock client response for created, linked, and
  conflict outcomes.

## Dependencies

- T003 gateway client.

## Provides To Downstream Tasks

- **T005:** end-to-end push behavior to validate.
- **T006:** documented CLI behavior.
