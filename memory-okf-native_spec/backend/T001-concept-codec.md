# T001 - Define OKF-native concept model and codec

**Depends on:** none

## Scope

Create pure Rust domain types for the joined memory/OKF concept, sources,
verification, generated metadata, lifecycle, extensions, relationships, and
`x-agent-memory`. Implement bounded OKF 0.2 parsing, validation, normalized
rendering, preservation rendering, semantic hashing, and deterministic diffs.

No SQLite, CLI, MCP, Dream, or gateway work belongs here.

## Acceptance criteria

- [ ] `type` is the only universally required OKF field and accepts arbitrary non-empty values.
- [ ] `memory_type` remains independent from `concept_type`.
- [ ] Existing plain text is accepted as the body without rewriting.
- [ ] Bare and list `verified` forms parse to one normalized representation.
- [ ] Unknown frontmatter fields survive parse/render/reparse.
- [ ] OKF 0.1 timestamp and Citations fallbacks warn but parse.
- [ ] Attested Computation metadata validates but cannot execute.
- [ ] Semantic hashes exclude operational caches and include all concept semantics.
- [ ] Size, nesting, alias, link, source, verification, and extension limits have stable diagnostics.
- [ ] Unit fixtures cover minimal, complete, extension-heavy, malformed, and hostile documents.

## Touch surface

- `memory/src/okf/`
- `memory/src/lib.rs`
- `memory/Cargo.toml`
- codec fixtures under `memory/tests/fixtures/okf/`

## Validation

- `cargo test -p agent-memory okf`
- `cargo fmt --all --check`
