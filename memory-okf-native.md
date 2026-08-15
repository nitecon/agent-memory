# OKF-Native Agent Memory

## Status

- Specification version: 1
- Date: 2026-08-15
- Target: `agent-memory` and `memory-dream`
- Compatibility target: Open Knowledge Format 0.2
- Baseline database schema: 8

## Summary

Agent-memory will make every durable memory an OKF-compatible concept whose
canonical representation lives in SQLite. The existing `memories.content`
text is the concept body; plain text is valid Markdown and does not need to be
copied into a file. Structured OKF fields, immutable revisions, provenance,
verification, lifecycle, and relationships are stored alongside the memory.

Handlers project the database concept as an OKF document and project a project
scope as a virtual OKF bundle. A caller can read or write virtual `*.md`
documents, navigate generated `index.md` views, inspect generated `log.md`
history, traverse links, and import or export physical bundles without making
files the source of truth.

SQLite provides the repository properties needed by the model: stable
identity, transactions, immutable revisions, diffs, actor provenance,
tombstones, conflict detection, query-generated indexes, and gateway
synchronization. Physical Markdown and Git remain optional interchange and
review surfaces.

This is a standalone agent-memory capability. It has no dependency on
agent-tools or any external graph/indexing service.

## Normative principles

1. A durable memory is the canonical concept. An exported Markdown file is a
   projection, never a second authority.
2. `memories.content` is the canonical body. Existing text remains valid
   without rewriting because plain text is valid Markdown.
3. `memory_type` remains the operational memory category
   (`user|feedback|project|reference`). `concept_type` is the independent,
   arbitrary OKF `type` used for domain semantics.
4. OKF 0.2 requires only a non-empty `type`. Existing memories migrate to a
   valid minimal concept without invented provenance or verification.
5. Every semantic change creates an immutable revision before the current
   projection changes.
6. Unknown OKF frontmatter fields round-trip losslessly.
7. Links, sources, actors, verification, status, and freshness are structured
   and queryable; rendered Markdown preserves their portable OKF form.
8. Dream may curate memory concepts, but it must preserve revision history and
   provenance and must invalidate verification after meaningful changes.
9. WorkingContext is transient handoff state, not a durable concept. It never
   appears in OKF handlers, graph traversal, export, or concept sync.
10. Attested Computation contracts may be stored, rendered, and validated but
    are never executed by agent-memory.

## Goals

1. Make all existing and future durable memories valid logical OKF concepts.
2. Expose lossless database-to-OKF and OKF-to-database handlers.
3. Provide virtual bundle navigation without requiring physical files.
4. Add immutable revision history, diffs, optimistic writes, and tombstones.
5. Add memory-to-memory and memory-to-resource relationships.
6. Use OKF metadata and graph neighborhoods in search, context, and Dream.
7. Extend CLI and MCP without breaking current callers.
8. Evolve gateway exchange to preserve the complete concept contract while
   remaining compatible with legacy memory payloads.
9. Provide explicit physical import/export for interoperability and review.
10. Prove migrations, round trips, concurrency, security, and retrieval with
    deterministic tests.

## Non-goals

- Requiring Markdown files, Git, agent-tools, a graph server, or another
  database.
- Indexing a repository or discovering external OKF bundles automatically.
- Turning WorkingContext into a concept.
- Treating verification or trust tier as authorization.
- Executing computation, executor, or attester metadata.
- Fetching URLs or resource references during parsing, rendering, search, or
  graph traversal.
- Replacing the current memory type and scope semantics.
- Automatically committing or pushing physical exports.

## Terminology

- **Concept:** one durable memory plus OKF metadata and body.
- **Current projection:** the query-efficient live fields in `memories` and
  concept side tables.
- **Revision:** an immutable semantic snapshot of a concept.
- **Virtual document:** the lossless OKF Markdown rendering of a concept.
- **Virtual bundle:** a project/global scope exposed through generated paths,
  documents, indexes, and logs.
- **Handler:** the internal read/write interface used by CLI, MCP, gateway, and
  tests; it does not imply a filesystem.
- **Relationship:** a directed edge from a memory concept to another memory,
  actor, or unresolved/external resource.

## Logical concept model

### Existing fields retained

```text
memories.id
memories.content
memories.tags
memories.project
memories.agent
memories.source_file
memories.created_at
memories.updated_at
memories.access_count
memories.embedding
memories.memory_type
memories.content_raw
memories.superseded_by
memories.condenser_version
memories.embedding_model
```

Existing command behavior continues to use these fields. New OKF behavior is
additive and is accessed through a joined domain model.

### `memory_concepts`

```text
memory_id               TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE
concept_type            TEXT NOT NULL CHECK(TRIM(concept_type) <> '')
title                   TEXT
description             TEXT
resource                TEXT
status                  TEXT NOT NULL DEFAULT 'stable'
                        CHECK(status IN ('draft','stable','deprecated'))
stale_after             TEXT
generated_by            TEXT
generated_at            TEXT
extensions_json         TEXT NOT NULL DEFAULT '{}'
raw_frontmatter         TEXT
current_revision        INTEGER NOT NULL DEFAULT 1
virtual_path            TEXT NOT NULL UNIQUE
created_at              TEXT NOT NULL
updated_at              TEXT NOT NULL
```

`concept_type` and `memory_type` are deliberately different. New stores default
`concept_type` to `Agent Memory/<memory_type>` unless the caller supplies a
domain type. Existing memories backfill the same deterministic value.

`raw_frontmatter` retains an imported representation when useful for lossless
round trips. Normalized fields remain canonical after a semantic update.
Unknown keys live in `extensions_json` and must be emitted again.

`stale_after` is an ISO `YYYY-MM-DD` date. Absence means no declared expiry.
`generated_by` follows OKF actor conventions when supplied. Migration must not
fabricate a human or model identity for legacy rows.

### `memory_revisions`

```text
id                      TEXT PRIMARY KEY
memory_id               TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE
revision                INTEGER NOT NULL
parent_revision         INTEGER
operation               TEXT NOT NULL
actor                   TEXT
snapshot_json           TEXT NOT NULL
content_hash            TEXT NOT NULL
created_at              TEXT NOT NULL
UNIQUE(memory_id, revision)
UNIQUE(memory_id, content_hash)
```

The canonical snapshot contains all semantic concept fields: body, tags,
memory category, scope, OKF normalized metadata, extensions, sources,
verifications, and asserted relationships. Access counts, cached embeddings,
gateway sync bookkeeping, and index timestamps are not semantic and are not
part of the hash.

Operations include `migrate`, `store`, `put`, `update`, `dream_condense`,
`dream_merge`, `dream_extract`, `move`, `copy`, `gateway_pull`, `import`, and
`forget`. Values are extensible.

Every semantic writer must:

1. prepare inference and embeddings outside a write transaction;
2. begin a short transaction;
3. verify the expected current revision when supplied;
4. insert or reuse the immutable snapshot;
5. update the current projection and revision pointer;
6. update normalized sources/verifications/relationships;
7. commit;
8. perform network synchronization after the local commit according to the
   existing gateway policy.

No transaction may remain open across LLM inference, embedding, filesystem
interaction, or network calls.

### `memory_sources`

```text
memory_id               TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE
source_key              TEXT NOT NULL
ordinal                 INTEGER NOT NULL
resource                TEXT NOT NULL
title                   TEXT
author                  TEXT
usage_count             INTEGER
usage_window_from       TEXT
usage_window_to         TEXT
last_modified           TEXT
metadata_json           TEXT NOT NULL DEFAULT '{}'
PRIMARY KEY(memory_id, source_key)
UNIQUE(memory_id, ordinal)
```

Source keys are stable joins for Markdown footnotes. Reordering never changes
claim attribution.

### `memory_verifications`

```text
id                      TEXT PRIMARY KEY
memory_id               TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE
actor                   TEXT NOT NULL
verified_at             TEXT NOT NULL
verification_kind       TEXT
metadata_json           TEXT NOT NULL DEFAULT '{}'
```

Trust tier is derived at read time:

- no verification: `unverified`;
- only non-`human:` actors: `machine-confirmed`;
- at least one `human:` actor: `human-reviewed`.

A meaningful body or semantic metadata change clears current verification
unless the same atomic operation includes replacement verification events.
Historical revisions preserve the old verification snapshot.

### `memory_relationships`

```text
id                      TEXT PRIMARY KEY
src_memory_id           TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE
dst_memory_id           TEXT REFERENCES memories(id) ON DELETE SET NULL
dst_ref                 TEXT NOT NULL
relation                TEXT NOT NULL
confidence              TEXT NOT NULL
producer                TEXT NOT NULL
source_revision         INTEGER NOT NULL
ordinal                 INTEGER
metadata_json           TEXT NOT NULL DEFAULT '{}'
created_at              TEXT NOT NULL
```

Initial relations:

```text
links_to, cites, derived_from, applies_to, generated_by,
verified_by, supersedes, contradicts, aliases
```

Relation values remain extensible. Portable document links render as Markdown.
Richer relationships render under `x-agent-memory.edges` so another OKF reader
can ignore the extension without losing standard conformance.

Relationships extracted from a rendered document are owned by the document
producer/revision. Re-parsing replaces only those extracted rows. Explicit and
Dream-produced relationships are not deleted by the Markdown extractor.

### Tombstones and audit

Existing gateway tombstone behavior remains. Add a concept audit event for
local deletion before the live rows cascade. A tombstone record contains the
memory ID, virtual path, project/scope, last revision/hash, deletion actor,
reason, and timestamp. Virtual `log.md` includes tombstones. Deleted concept
documents return not found unless history access is explicit.

## Existing-memory migration

Migration is automatic, transactional, restartable, and idempotent.

For each live memory without `memory_concepts`:

1. create `concept_type = "Agent Memory/<memory_type>"` using `user` when the
   legacy type is absent;
2. set effective `status = stable`;
3. derive stable `virtual_path = /memories/<uuid>.md` within its scope bundle;
4. leave title, description, resource, stale date, generator, sources,
   verification, and extensions absent;
5. create revision 1 with operation `migrate` from the existing semantic row;
6. retain the body byte-for-byte;
7. retain existing IDs, timestamps, tags, embeddings, gateway mappings, and
   supersession state.

The migration must not call the embedding model. Fresh databases create all
tables directly. Migration tests start from every supported historical schema
fixture, including schema 8.

## Virtual identity and bundles

### Canonical URIs

```text
memory://project/<percent-encoded-project>/<uuid>
memory://global/<uuid>
memory://unscoped/<uuid>
actor:<actor-id>
external:<normalized-reference>
```

The UUID is the durable native identity. Project moves alter the canonical
scope URI and record the prior URI as an alias relationship. Copies mint a new
UUID and a `derived_from` relationship.

### Bundle roots

Each scope is a virtual bundle:

```text
okf+memory://project/<percent-encoded-project>/
okf+memory://global/
okf+memory://unscoped/
```

Project bundle example:

```text
/
  index.md
  log.md
  memories/
    <uuid>.md
  types/
    <encoded-concept-type>/index.md
  tags/
    <encoded-tag>/index.md
```

Only `/memories/<uuid>.md` is a writable concept path. Index and log paths are
generated, read-only views. Type and tag directories are query views, not
duplicate concepts.

### Handler contract

```text
OkfDocumentHandler
  render(memory_id | canonical_uri) -> RenderedDocument
  parse(document_text) -> ParsedConcept
  put(target, parsed, expected_revision?) -> PutResult
  validate(document_text) -> Diagnostics

OkfBundleHandler
  read(bundle, virtual_path) -> VirtualEntry
  list(bundle, virtual_path) -> [VirtualEntrySummary]
  index(bundle, filters?) -> RenderedDocument
  log(bundle, cursor?, limit?) -> RenderedDocument
```

Handlers are ordinary Rust interfaces used by CLI and MCP. They do not access
agent-tools or require FUSE, a mounted filesystem, or physical Markdown.

## OKF document projection

### Normalized rendering

The handler emits deterministic UTF-8:

```markdown
---
type: Project Decision
title: Gateway conflict policy
tags: [gateway, synchronization]
status: stable
generated:
  by: process:memory-dream
  at: 2026-08-15T14:00:00Z
x-agent-memory:
  id: 01234567-89ab-cdef-0123-456789abcdef
  revision: 3
  memory_type: project
  project: agent-memory
---

The gateway preserves local content when a remote conflict is detected.
```

Normative rules:

- `type` is always present and non-empty.
- Optional absent fields are omitted.
- `status: stable` may be emitted explicitly for deterministic self-description.
- `verified` may be accepted as a mapping or list but renders as a list.
- Sources render in stable ordinal order.
- Tags render in stored order after trimming duplicates.
- Unknown extension keys are preserved; reserved normalized keys win if an
  extension attempts to shadow them.
- `x-agent-memory.id`, `revision`, `memory_type`, and scope metadata guarantee a
  lossless round trip to the native record.
- Body bytes are preserved apart from one normalized frontmatter/body boundary
  newline in normalized-render mode.
- A preservation mode may reuse raw frontmatter when no semantic field changed.

### Parsing and put semantics

The parser accepts OKF 0.2 and warned 0.1 fallbacks. It validates shapes and
bounds but tolerates unknown types, unknown keys, absent optional metadata,
broken links, and missing index/log documents.

`put` supports:

- create at a newly allocated memory UUID;
- update an existing concept by `x-agent-memory.id`, canonical URI, or writable
  virtual path;
- optional `expected_revision` compare-and-swap;
- dry-run diff without mutation;
- preservation of operational fields not represented by OKF unless explicitly
  included in the `x-agent-memory` extension.

An ID in document metadata must match the target when both are supplied.
Mismatches are rejected, never redirected silently.

## Virtual indexes and logs

Root `index.md` is generated from live concepts. It groups by concept type by
default and links to `/memories/<uuid>.md`. Each entry includes title or short
ID, description or bounded body preview, status, and stale marker. Type/tag
indexes apply deterministic filters and ordering.

`log.md` is generated from immutable revision and tombstone events, newest
first, with ISO date headings and bounded pagination. Entries link to the live
concept or history identifier and include operation, actor when known, and a
short semantic summary. It is not free-form canonical state.

## Graph behavior

Markdown links between virtual concept paths resolve to `dst_memory_id`.
Canonical memory URIs also resolve. External URLs and unknown references remain
unresolved `dst_ref` values and are never fetched.

Graph reads support direction, relation filter, depth, fan-out, and result
limits. Default depth is one. Traversal records path and relation labels and
detects cycles. Broken and ambiguous references produce diagnostics.

The graph is used for retrieval expansion only after textual candidates are
selected. It cannot make an irrelevant concept relevant solely through high
degree.

## Search and context

### Search corpus

Existing memory body retrieval remains. Extend lexical candidate text with
title, description, concept type, tags, and optionally heading paths. Short
memories remain one segment. Longer bodies may be segmented by Markdown
headings with bounded fallback chunks; segments are derived and rebuildable.

Embeddings use the existing model/cache and attach to the current semantic
revision. Revision creation and DB transactions never wait on embedding.

### Ranking

The current BM25/vector RRF, optional cross-encoder reranking, and project/global
boosts remain the base. Add transparent modifiers:

- deprecated concepts excluded by default;
- stale concepts labelled and mildly down-ranked;
- draft and unverified concepts remain available with labels;
- exact concept type/tag filters are hard filters when requested;
- direct relevant relationships receive a bounded relation-specific boost;
- graph distance decays and defaults to one hop;
- resource-level diversity prevents adjacent chunks from crowding results.

Trust is advisory and shown as signals. It never overrides scope or access
rules.

### Context and hook rendering

Current grouped memory sections remain compatible. Each result may add compact
attributes for concept type, status, stale state, trust tier, revision, and
canonical URI. Graph neighbors are nested or separately labelled and count
against the same deterministic output budget.

The hook never performs migrations, Dream, network fetches, or unbounded graph
work. Failures fall back to current flat memory results.

## CLI contract

Add:

```text
memory okf validate <FILE|->
memory okf get <ID|URI|VIRTUAL_PATH>
memory okf put <ID|URI|VIRTUAL_PATH|new> [--file FILE|-]
               [--expect-revision N] [--dry-run]
memory okf read <BUNDLE_URI> <VIRTUAL_PATH>
memory okf list <BUNDLE_URI> [VIRTUAL_PATH]
memory okf index <BUNDLE_URI> [--type TYPE] [--tag TAG]
memory okf log <BUNDLE_URI> [--cursor CURSOR] [-k N]
memory okf history <ID> [-k N]
memory okf diff <ID> <REV_A> <REV_B>
memory okf graph <ID|URI> [--relation REL] [--direction in|out|both]
                 [--depth N] [-k N]
memory okf export <BUNDLE_URI> <TARGET> [--id ID ...] [--dry-run]
memory okf import <SOURCE> [--project P|--scope global] [--dry-run]
```

Existing `store` and `update` gain optional structured OKF inputs without
requiring them. Existing output stays compatible. New output uses compact
light-XML for normal actions and full Markdown only for explicit document reads.

Physical import/export uses the same handlers. Export never commits or pushes.
Import defaults to dry-run when identity/conflict intent is ambiguous.

## MCP contract

Expose read tools and resources:

```text
memory_okf_get
memory_okf_validate
memory_okf_list
memory_okf_history
memory_okf_diff
memory_okf_graph
okf+memory://project/... virtual resources
```

Expose `memory_okf_put` only with explicit content and optional expected
revision. Its description must state that it mutates durable memory. Physical
export/import is CLI-only in v1.

## Dream contract

Dream reads the joined concept model. Its prompts may use concept type,
provenance, lifecycle, verification, and graph neighbors.

For every applied semantic action:

- create an immutable revision with the exact operation and producer;
- retain sources and extensions unless the decision explicitly changes them;
- retain or deliberately rewrite relationships with provenance;
- clear current verification on meaningful change unless replacement
  verification is supplied;
- generate `derived_from` for extracts/copies;
- generate `supersedes` for merge/replacement decisions;
- surface contradictions rather than silently deduplicating them;
- prepare inference and embeddings before the short write transaction.

Dry-run mode creates no revisions, relationships, tombstones, or timestamps.

## Gateway exchange

The gateway remains optional, but when configured it should eventually carry
the full concept semantics.

Add a versioned optional `okf` envelope to `GatewayMemory`:

```text
okf:
  version
  concept_type
  title
  description
  resource
  status
  stale_after
  generated
  sources
  verified
  relationships
  extensions
  concept_revision
```

Compatibility requirements:

- legacy payloads without `okf` import as minimal concepts;
- clients do not send the envelope until gateway capability/version support is
  known, unless the endpoint contract explicitly accepts unknown optional data;
- new clients preserve unknown OKF extensions through pull/update/push;
- concept hash covers the semantic envelope and body, while the legacy content
  hash remains available during transition;
- optimistic base revisions and tombstone rules remain;
- WorkingContext remains excluded;
- global/project scope behavior remains unchanged.

Gateway work may be feature-gated until the external endpoint supports the
contract. Local OKF behavior must not depend on gateway rollout.

## Security and limits

- Treat document text, YAML, extensions, links, imported archives, and gateway
  envelopes as untrusted.
- Bound document size, YAML nesting/aliases, field lengths, sources,
  verifications, relationships, segments, graph depth/fan-out, diffs, and
  rendered output.
- Reject NULs, invalid UTF-8, unsafe paths, archive traversal, symlink escape,
  duplicate reserved keys, and ID/target mismatches.
- Parameterize SQL and validate FTS expressions.
- Never fetch referenced URLs.
- Never execute computation metadata.
- Physical export/import requires explicit paths and conflict-safe writes.
- Secret detection applies before physical export and gateway concept sync.

## Observability

Mutation output includes memory ID, revision, operation, concept type, scope,
content hash, and sync outcome. Validation reports errors and warnings with
stable codes and source spans. Graph output labels unresolved/ambiguous edges.
Migration reports rows backfilled and verifies counts before commit.

Metrics or logs distinguish parsing, rendering, embedding, search, reranking,
graph expansion, Dream, and gateway time. Sensitive bodies and secret fields
are never logged.

## Compatibility and rollout

1. Land domain types and pure parser/renderer tests.
2. Land schema migration and backfill behind current behavior.
3. Route all semantic writers through one revision-aware service.
4. Add virtual document and bundle reads.
5. Add explicit writes and graph operations.
6. Extend search/context and MCP.
7. Update Dream.
8. Add capability-gated gateway envelope.
9. Complete import/export, end-to-end, security, and performance validation.
10. Document and publish the API context.

At every step, a database without new metadata and callers using only existing
commands must behave as before.

## End-to-end scenarios

1. **Legacy migration:** a schema-8 memory becomes a valid minimal concept with
   identical body, ID, scope, tags, timestamps, embedding, and gateway mapping.
2. **Virtual read:** `memory okf get` renders a conformant document without
   creating a file.
3. **Round trip:** render, parse, and dry-run put produces no semantic diff;
   unknown metadata survives.
4. **Optimistic update:** a matching expected revision succeeds and a stale one
   conflicts without mutation.
5. **History and diff:** updates create immutable revisions and deterministic
   semantic diffs.
6. **Virtual bundle:** root/type/tag indexes and log are generated from database
   queries and point to writable memory paths.
7. **Graph:** links between virtual documents resolve; broken links remain
   diagnosable; bounded traversal handles cycles.
8. **Search:** title/type/body matches and a relevant one-hop neighbor surface
   without regressing legacy body-only fixtures.
9. **Dream:** condensation creates a revision, preserves sources/extensions,
   clears verification, and creates derivation/supersession edges as needed.
10. **Gateway compatibility:** legacy and concept-aware fixtures round-trip,
    with capability gating and unchanged conflict/tombstone behavior.
11. **Physical interchange:** explicit export/import round-trips a bundle but
    the database remains canonical.
12. **Security:** hostile YAML, oversized inputs, unsafe paths, external URLs,
    and computation metadata cannot escape limits or execute.
13. **Concurrency:** no write transaction spans inference, embedding, or
    network calls; concurrent readers remain available under WAL.
14. **WorkingContext isolation:** no OKF command, resource, graph, export, or
    gateway envelope exposes WorkingContext.

## Definition of done

- All manifest tasks are complete.
- Existing CLI/MCP/gateway behavior remains backward compatible.
- Schema migration is transactional, restartable, and covered from historical
  fixtures.
- Pure OKF codec and virtual bundle round-trip fixtures pass.
- Revision, CAS, history, diff, graph, Dream, gateway, security, and hook tests
  pass.
- `cargo fmt --all --check` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- `cargo test --workspace` passes.
- `cargo build --workspace` passes.
- README, CLI help, MCP descriptions, and `.agent/api/memory.yaml` document the
  final behavior and API context is validated and published.
