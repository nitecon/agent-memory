# T004 - Implement virtual document and bundle handlers

**Depends on:** T003

## Scope

Implement `OkfDocumentHandler` and `OkfBundleHandler` over SQLite. Render and
put concept documents, read/list virtual paths, and generate root/type/tag
`index.md` plus paginated `log.md`. No physical file is needed.

## Acceptance criteria

- [ ] `/memories/<uuid>.md` renders a deterministic, conformant document from the canonical DB concept.
- [ ] Render/parse/dry-run-put is a semantic no-op and preserves unknown fields.
- [ ] Put supports create/update, expected revision, target/ID consistency, and dry-run diff.
- [ ] Root/type/tag indexes are deterministic query views with stable links and bounded previews.
- [ ] Log is generated from revisions/tombstones with ISO date headings and pagination.
- [ ] Index/log paths are read-only; only memory document paths can be put.
- [ ] Project, global, and unscoped bundle URIs are isolated.
- [ ] WorkingContext is unreachable from every handler.

## Touch surface

- `memory/src/okf/handlers.rs`
- `memory/src/okf/bundle.rs`
- `memory/src/render/`
- handler integration tests

## Validation

- virtual path, bundle isolation, round-trip, CAS, index, and log fixtures
