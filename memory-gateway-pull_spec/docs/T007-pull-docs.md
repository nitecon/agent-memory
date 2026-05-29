# T007 - Document memory pull workflow

**Team:** docs
**Phase:** 3
**Depends on:** T005
**Status:** todo

## Scope

**In:** Document the pull workflow and user-visible outcomes.

**Out:** Gateway server API documentation.

## Source references

- `memory-gateway-exchange.md`
- Existing CLI help in `memory/src/cli.rs`

## Deliverables

1. **README update** - short section for project memory gateway pull.
2. **CLI help text** - explains status and mutation behavior.

## Implementation Notes

- Keep documentation concise and operational.
- State that pull does not merge semantic near-duplicates automatically.

## Acceptance Criteria

- [ ] README or CLI help documents `memory pull status` and `memory pull`.
- [ ] Docs state project-only scope and explicitly exclude global memories and WorkingContext.
- [ ] Docs explain imported, updated, linked, tombstone, conflict, and rejected outcomes.
- [ ] Docs state that semantic near-duplicates are not silently merged during pull.

## Validation Plan

- Run `cargo test -p memory`.
- Manually inspect `memory pull --help` and `memory pull status --help`.

## Dependencies

- T005 command behavior.

## Provides To Downstream Tasks

- User-facing guidance for operating the pull flow safely.
