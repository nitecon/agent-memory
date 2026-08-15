# T009 - Make Dream curation revision- and provenance-aware

**Depends on:** T003, T005

## Scope

Move Dream semantic mutations onto the concept service and enrich review/
condensation with OKF metadata and graph context.

## Acceptance criteria

- [ ] Every applied Dream semantic action creates the correct immutable revision and operation actor.
- [ ] Sources, extensions, and unrelated relationships survive condensation.
- [ ] Meaningful rewrites clear verification unless replacement verification is explicit.
- [ ] Extract/copy and merge/replacement actions create derivation/supersession edges.
- [ ] Contradictions surface as findings rather than silent deduplication.
- [ ] Dry-run creates no revisions, edges, tombstones, timestamps, or gateway writes.
- [ ] Inference and embeddings complete before short DB write transactions.
- [ ] Existing Dream gateway tombstone/update retry behavior remains compatible.

## Touch surface

- `memory-dream/src/dream/`
- `memory/src/concepts/`
- Dream unit/integration tests
