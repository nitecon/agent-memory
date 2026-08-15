# T002 - Add concept, revision, provenance, and graph schema

**Depends on:** T001

## Scope

Add the next schema migration for `memory_concepts`, `memory_revisions`,
`memory_sources`, `memory_verifications`, `memory_relationships`, concept audit
tombstones, and any derived indexes. Backfill every existing durable memory as
a minimal valid concept and immutable revision 1 without re-embedding.

## Acceptance criteria

- [ ] Fresh databases create the complete schema with foreign keys and constraints.
- [ ] Schema-8 and all retained historical fixtures upgrade transactionally and idempotently.
- [ ] Body bytes, IDs, tags, scopes, timestamps, embeddings, supersession, and gateway metadata are unchanged.
- [ ] Backfill uses deterministic `Agent Memory/<memory_type>` concept types and stable `/memories/<uuid>.md` paths.
- [ ] Migration invents no generator, source, or verification identity.
- [ ] Snapshot/hash and current revision constraints reject inconsistent state.
- [ ] WorkingContext receives no concept row.
- [ ] Migration performs no model, filesystem, or network work.

## Touch surface

- `memory/src/db/mod.rs`
- `memory/src/db/models.rs`
- `memory/src/db/queries.rs`
- migration fixtures/tests

## Validation

- `cargo test -p agent-memory migration`
- inspect foreign-key and uniqueness behavior under rollback/reopen tests
