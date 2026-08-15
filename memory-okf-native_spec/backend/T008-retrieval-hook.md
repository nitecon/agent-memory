# T008 - Integrate OKF metadata and graph expansion into retrieval and hooks

**Depends on:** T003, T005

## Scope

Extend lexical/vector candidate text, filters, ranking, result models, context
rendering, and the existing prompt hook with concept metadata and bounded graph
neighbors while retaining current recall behavior.

## Acceptance criteria

- [ ] Title, description, concept type, tags, and body contribute to retrieval.
- [ ] Long bodies use rebuildable bounded heading segments; short memories remain cheap.
- [ ] Base BM25/vector RRF, reranker, and scope boosts remain compatible.
- [ ] Deprecated is excluded by default; draft/stale/unverified remain labelled and usable.
- [ ] Type/tag filters and resource-level diversity behave deterministically.
- [ ] Graph expansion is text-seeded, one hop by default, strictly bounded, and cycle-safe.
- [ ] Hook output has deterministic total and neighbor budgets and fails back to flat memory results.
- [ ] Fixed relevance fixtures show no material regression for legacy body-only searches.
- [ ] Access counts increment once per surfaced resource, not per internal segment.

## Touch surface

- `memory/src/search/`
- `memory/src/hook.rs`
- `memory/src/render/`
- `memory/src/cli.rs`
- `memory/src/mcp.rs`
