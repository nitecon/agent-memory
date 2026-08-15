# T006 - Expose OKF-native CLI and physical interchange

**Depends on:** T004, T005

## Scope

Add the `memory okf` CLI family for validate, get, put, read, list, index, log,
history, diff, graph, import, and export. Add optional structured OKF inputs to
store/update while preserving existing argument and output behavior.

## Acceptance criteria

- [ ] All commands in the source specification parse and have compact help.
- [ ] Explicit document reads return Markdown; action results use current light-XML conventions.
- [ ] Put/import/export support dry-run and conflict-safe writes.
- [ ] Export/import use the same handlers and round-trip deterministic physical bundles.
- [ ] Export never commits, pushes, follows symlinks outside target, or includes WorkingContext.
- [ ] Secret checks run before physical export.
- [ ] Legacy store/search/context/update invocations and output fixtures remain unchanged.
- [ ] Shell-injection and unsafe-path fixtures pass cross-platform.

## Touch surface

- `memory/src/cli.rs`
- `memory/src/render/`
- `memory/src/okf/interchange.rs`
- CLI integration tests and fixtures
