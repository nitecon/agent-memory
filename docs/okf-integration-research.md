# OKF Research Decision

The research converged on an OKF-native memory model rather than an external
file corpus or integration with another product.

Every durable SQLite memory is the canonical OKF concept. Its existing text is
the concept body; structured OKF metadata, immutable revisions, provenance,
verification, lifecycle, and relationships live in the database. Handlers
render and parse virtual Markdown documents and generate virtual `index.md` and
`log.md` bundle views. Physical files and Git are optional interchange/review
surfaces, not storage requirements.

The normative implementation specification is
[`memory-okf-native.md`](../memory-okf-native.md). The decomposed implementation
manifest is
[`memory-okf-native_spec/manifest.yaml`](../memory-okf-native_spec/manifest.yaml).
