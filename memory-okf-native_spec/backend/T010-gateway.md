# T010 - Add capability-gated OKF gateway envelope

**Depends on:** T003

## Scope

Define optional versioned OKF concept metadata on `GatewayMemory`, capability
gating, semantic hashing, preservation, push/pull planning, conflicts, and
tombstones. Local OKF functionality must remain independent of rollout.

## Acceptance criteria

- [ ] Legacy payloads without `okf` import as deterministic minimal concepts.
- [ ] New envelope covers normalized fields, sources, verification, relationships, extensions, and concept revision.
- [ ] Clients send the envelope only when endpoint capability/contract permits it.
- [ ] Unknown extension data survives pull, local no-op, and later push.
- [ ] Semantic and legacy hashes coexist during transition without false conflicts.
- [ ] Existing project/global scope, optimistic revision, auto-sync, delete queue, and tombstone behavior remains.
- [ ] WorkingContext is excluded.
- [ ] Contract fixtures cover old client/new gateway and new client/old gateway combinations.

## Touch surface

- `memory/src/sync.rs`
- `memory/src/gateway_sync.rs`
- `memory/src/cli.rs`
- gateway contract tests

## External dependency

If gateway changes are required, create delegated gateway tasks before enabling
the envelope by default. Feature-gated local code and fixtures may land first.
