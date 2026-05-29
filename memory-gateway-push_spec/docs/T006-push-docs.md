# T006 - Document memory push workflow

**Team:** docs
**Phase:** 3
**Depends on:** T004
**Status:** todo

## Scope

**In:** Document the push workflow and user-visible outcomes.

**Out:** Gateway server API documentation.

## Source references

- `memory-gateway-exchange.md`
- Existing CLI help in `memory/src/cli.rs`

## Deliverables

1. **README update** - short section for project memory gateway push.
2. **CLI help text** - explains status and mutation behavior.

## Implementation Notes

- Keep documentation concise and operational.
- State that global memories and WorkingContext are excluded.

## Acceptance Criteria

- [ ] README or CLI help documents `memory push status` and `memory push`.
- [ ] Docs state project-only scope and explicitly exclude global memories and WorkingContext.
- [ ] Docs explain created, updated, linked, conflict, and rejected outcomes.
- [ ] Docs state that push conflicts must be resolved before a later push can update the gateway record.

## Validation Plan

- Run `cargo test -p memory`.
- Manually inspect `memory push --help` and `memory push status --help`.

## Dependencies

- T004 command behavior.

## Provides To Downstream Tasks

- User-facing guidance for operating the push flow safely.
