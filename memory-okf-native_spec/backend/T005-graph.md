# T005 - Implement relationships and bounded graph traversal

**Depends on:** T003, T004

## Scope

Persist explicit and extracted relationships, resolve virtual document links,
preserve unresolved references, and provide deterministic bounded traversal.

## Acceptance criteria

- [ ] Markdown links and sources create producer-owned relationships for the current revision.
- [ ] Reparse replaces only relationships owned by that producer/revision.
- [ ] Virtual paths and canonical memory URIs resolve deterministically.
- [ ] Broken/ambiguous/external references remain diagnostic and are never fetched.
- [ ] Traversal supports direction, relation, depth, fan-out, result limits, paths, and cycles.
- [ ] Rich typed edges round-trip under `x-agent-memory.edges`.
- [ ] Existing `superseded_by` state projects consistently to `supersedes`.
- [ ] Parameterized query and adversarial fan-out tests pass.

## Touch surface

- `memory/src/okf/links.rs`
- `memory/src/concepts/graph.rs`
- `memory/src/db/queries.rs`
- graph fixtures/tests
