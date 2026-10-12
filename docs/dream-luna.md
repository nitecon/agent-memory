# Dream with Luna

Dream supports `gpt-6-luna` through its existing headless stdin transport and
the authenticated Codex CLI. No Ollama model rename or new adapter is needed.
[Official model guidance](https://learn.chatgpt.com/docs/models) lists Luna;
account availability must still be checked on the scheduled host.
[Non-interactive Codex](https://learn.chatgpt.com/docs/non-interactive-mode)
supports stdin prompts and separates final stdout from stderr diagnostics.

## Validated behavior

On 2026-10-12, Codex CLI 0.162.1 accepted the exact model `gpt-6-luna` with
provider `openai`. The following opt-in tests passed against that model:

- Single-memory condensation preserved useful guidance under the three-way
  response contract.
- A 70-memory project review received a 160,337-byte prompt through stdin,
  returned complete decisions for 70/70 IDs, merged a duplicate, dropped
  release-log noise, and preserved the CUDA compatibility constraint.
- The review applied its decisions in dry-run mode to an isolated in-memory
  fixture. The immutable revision count stayed unchanged; no gateway was used.

The production inference identity for this command is
`headless/codex/gpt-6-luna`. Dream appends its policy hash to the processing
stamp. Children receive `AGENT_MEMORY_HOOK=off`, `AGENT_TOOLS_HOOK=off`, and
`MEMORY_DREAM_HEADLESS=1`. The command ignores user CLI configuration and
repository instruction files, uses a read-only sandbox, and asks for no tools.

Regression tests also verify that successful keep and rewrite decisions skip
two subsequent ordinary passes without further inference, revisions or gateway
writes, while failures remain retryable. New evidence can replace an older
processed memory with combined context and leave unrelated memories unchanged.
Single rewrites allow at most 50% growth over the original baseline; merged
output allows at most 75% of the combined source lengths. Limits count Unicode
characters and round down. Model instructions forbid unsupported additions.

## SRE cutover

SRE owns the scheduled host configuration and adapter deployment. After
installing the validated release, update the existing backend/headless sections
of `dream.toml`, preserving its other sections:

```toml
[backend]
mode = "headless"

[headless]
model = "gpt-6-luna"
command = """codex exec --ignore-user-config --ephemeral --sandbox read-only -c project_doc_max_bytes=0 -c 'model_reasoning_effort="low"' -c 'developer_instructions="Classify only the supplied memories. Never call tools or inspect files. Return only the requested output."' --model {model} - {prompt_stdin}"""
timeout_ms = 600000
```

Before changing the schedule, verify the scheduled user's Codex authentication
and PATH, then preview with `memory-dream --dry-run --backend headless
--headless-model gpt-6-luna --command-override '<command above>'`. Use the full
command string from the TOML snippet, without TOML delimiters. Verify complete
review decisions and truthful failures on representative host memories. A model
or policy change intentionally makes prior stamps stale for the first pass.

After the authorized host cutover, observe two scheduled passes. Unchanged
successful processing should disappear from ordinary work; failures should
remain retryable, and fresh related evidence should still permit consolidation.
Keep the SRE tracker open until these observations are complete.

To rerun the isolated live validation from this repository:

```bash
cargo test -p memory-dream --test luna_validation -- --ignored --nocapture
```

These tests require authenticated model access and incur model usage. They do
not run in ordinary CI. Their synthetic evidence does not establish that Luna
resolves every production refusal or review/transport failure. Gateway revision
reconciliation remains SRE task `01a05f36-c76b-7bc1-939b-d2098502f887`;
failure-counter reporting remains `01a065a7-0e1e-7c23-a5a9-5128e3fcab03`.
