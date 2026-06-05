# agent-memory

Persistent hybrid-search memory system for AI coding agents. Replaces markdown-based memory with selective retrieval, cross-project search, and agent-scoped memories that scale without context cost.

## Architecture

- **SQLite** -- single-file backing store, portable, zero config
- **fastembed-rs** -- local embeddings via all-MiniLM-L6-v2 ONNX model (semantic similarity, no API calls)
- **Hybrid ranking** -- BM25 (FTS5) + cosine similarity combined via Reciprocal Rank Fusion (RRF)
- **MCP server** -- stdio JSON-RPC server for native Claude Code tool integration
- **CLI** -- direct command-line interface for humans, scripts, and AI agents

## Install

### Linux / macOS

```bash
curl -fsSL https://raw.githubusercontent.com/nitecon/agent-memory/refs/heads/main/install.sh | sudo bash
```

Installs **two** binaries to `/opt/agentic/bin/` (symlinked into `/usr/local/bin/`):

- `memory` — the main CLI + MCP server
- `memory-dream` — offline batch compactor (condense + dedup, see [Dream compactor](#dream-compactor-offline-condensation--dedup) below)

### Windows (PowerShell as Administrator)

```powershell
irm https://raw.githubusercontent.com/nitecon/agent-memory/refs/heads/main/install.ps1 | iex
```

Installs `memory.exe` + `memory-dream.exe` to `%USERPROFILE%\.agentic\bin\` and adds the directory to your PATH.

### From source

This is a Cargo workspace. The default build produces both binaries:

```bash
cargo build --release
# → target/release/memory
# → target/release/memory-dream
```

(Windows adds the `.exe` suffix.)

First `memory` invocation downloads the embedding model (~80MB, cached alongside the database). First `memory-dream --pull` downloads the gemma3 weights (~2GB, same cache directory).

### Release archives

Each release tag produces a single combined archive per platform:

```
agent-memory-linux-x86_64.tar.gz        # memory + memory-dream
agent-memory-linux-aarch64.tar.gz
agent-memory-macos-x86_64.tar.gz
agent-memory-macos-aarch64.tar.gz
agent-memory-windows-x86_64.zip
```

Asset filenames are intentionally tag-less so an older binary can resolve the download URL for any future release — `memory update` works across version jumps without knowing the new tag ahead of time.

Archive size is ~70MB (candle + tokenizers add weight to `memory-dream`). The model weights are **not** shipped in the archive — they're downloaded on demand via `memory-dream --pull`. Users who never run `memory-dream` pay the ~28MB of disk it takes up but incur zero cognitive overhead; `memory update` force-bundles both binaries on every upgrade so install and updater logic stay symmetric.

## Database location

The database path is resolved in this order:

| Priority | Condition | Path |
|----------|-----------|------|
| 1 | `AGENT_MEMORY_DIR` env var is set | `$AGENT_MEMORY_DIR/memory.db` |
| 2 | `~/.agentic/memory.db` exists | `~/.agentic/memory.db` (user-local) |
| 3 | writable `/opt/agentic/memory.db` exists (Linux/macOS) | `/opt/agentic/memory.db` (legacy shared DB) |
| 4 | Default | `~/.agentic/memory.db` |

The model cache and any auxiliary data are stored alongside the database in the same directory.

Fresh installs default to the user-writable `~/.agentic/` directory so first-run model downloads and database writes do not depend on `/opt` permissions. Existing shared Linux/macOS installs that already have a writable `/opt/agentic/memory.db` continue to use that database. Set `AGENT_MEMORY_DIR` when you need an explicit shared or test data directory.

## Recommended usage: CLI-first

Calling the `memory` binary directly is the recommended approach. It is just as fast as MCP mode and avoids the overhead of running a persistent server process. The fastest way to teach your agent to use it is the `memory setup` command — it bundles an interactive checklist that injects the rules block into your agent rule files and installs a Claude Code skill that auto-advertises the CLI to every session.

### WorkingContext handoff

WorkingContext is transient per-project handoff state. `memory context` renders
the current project's WorkingContext before ranked memories; durable lessons
still belong in `memory store`.

```bash
# Read the current handoff; absent state is present="false".
memory working get

# Replace the handoff. Use "-" for multiline stdin.
memory working set "Current state and exact next step"
memory working set - < handoff.md

# Delete the handoff when the project thread completes.
memory working clear
```

WorkingContext is project-only, rejects `__global__`, and is capped at 65,536
characters so every future `memory context` call stays bounded.
Project-wide `memory move --from X --to Y` transfers the handoff to `Y`;
moving to an empty target clears it. `memory copy` does not duplicate
WorkingContext, and `memory projects` lists durable-memory projects only.

### Project memory gateway exchange

`memory push` and `memory pull` exchange only durable memories for the current
project ident. Global memories (`__global__`) and WorkingContext are excluded.

Configure the gateway once with either `memory setup gateway` or the equivalent
`agent-tools setup gateway`. Both commands write the shared
`~/.agentic/agent-tools/gateway.conf` file that `memory push` and
`memory pull` read.

When a gateway URL and API key are configured, `memory store` automatically
syncs project-scoped durable memories by pushing pending local changes and then
pulling remote changes. The automatic sync is best-effort: the memory remains
saved locally if the gateway is unavailable, and the command emits a retry hint.
Disable it by setting `AGENT_MEMORY_GATEWAY_AUTO_SYNC=false` in the shared
gateway config; `memory setup gateway` prompts for this setting.

Environment variables still override the shared file when set:

```bash
export AGENT_MEMORY_GATEWAY_URL="https://gateway.example"
export AGENT_MEMORY_GATEWAY_API_KEY="..."
export AGENT_MEMORY_GATEWAY_AUTO_SYNC="false" # optional opt-out

# Fallback names also work:
export AGENT_GATEWAY_URL="https://gateway.example"
export AGENT_GATEWAY_API_KEY="..."

# agent-tools-compatible names also work:
export GATEWAY_URL="https://gateway.example"
export GATEWAY_API_KEY="..."
```

```bash
# Local, read-only preview of project memories that would be uploaded.
memory push status

# Upload pending project memories. Successful created/updated/linked results
# record gateway IDs and server revisions locally.
memory push

# Gateway-backed preview of remote project-memory diffs.
memory pull status

# Import/link/update non-conflicting remote project memories. Conflicts are
# reported without overwriting local content.
memory pull
```

Push outcomes are `created`, `updated`, `linked`, `conflict`, or `rejected`.
Pull outcomes are `import`, `update`, `link`, `tombstone`, `conflict`,
`skipped`, or `rejected`. Semantic near-duplicates are not silently merged
during pull; use the dream/consolidation flow after import when memories need
curation.

### Auto-install the agent protocol

`memory setup` is now a small subcommand family:

| Command | Behavior |
|---------|----------|
| `memory setup` | Interactive checklist: shows the install state of each component (gateway, rules, skill) and lets you pick which to (re)install |
| `memory setup rules [flags]` | Inject the `<memory-rules>` block into known agent rule files (CLAUDE.md, GEMINI.md, AGENTS.md) |
| `memory setup rules --remove` | Strip the `<memory-rules>` block and reverse every paired native-memory-disable write (Claude `autoMemoryEnabled`, Gemini `excludeTools: save_memory`, Codex `[features] memories`) |
| `memory setup skill [flags]` | Install `SKILL.md` under **every** known agent frontend — `~/.claude/skills/agent-memory/` (Claude Code, tool-native) and `~/.agents/skills/agent-memory/` (cross-agent alias read by Gemini CLI and Codex) — so each session auto-loads a ~100-token description that nudges the model toward the CLI |
| `memory setup skill --remove` | Delete the installed `SKILL.md` from every known target. Missing files are silently skipped — parity with `setup rules --remove` |
| `memory setup all [-y]` | Run gateway → rules → skill non-interactively (use `-y` / `--yes` to skip confirmation). Rules land before skill so a human reading a freshly-updated CLAUDE.md sees the new rules ahead of the model discovering the skill |

```bash
# Bare invocation: 3-item interactive checklist (gateway + rules + skill).
memory setup

# Rules only — detects ~/.claude/CLAUDE.md, ~/.gemini/GEMINI.md,
# ~/.codex/AGENTS.md, ~/.config/codex/AGENTS.md.
memory setup rules               # detect + prompt
memory setup rules --all         # update every detected file
memory setup rules --target ~/.claude/CLAUDE.md
memory setup rules --dry-run     # preview, don't write
memory setup rules --print       # emit just the <memory-rules> block
memory setup rules --all --remove  # uninstall: strip block + reverse every native-memory-disable write

# Skill only — installs SKILL.md to every known frontend:
#   ~/.claude/skills/agent-memory/SKILL.md   (Claude Code, tool-native)
#   ~/.agents/skills/agent-memory/SKILL.md   (cross-agent — Gemini CLI + Codex)
memory setup skill
memory setup skill --dry-run
memory setup skill --print
memory setup skill --remove     # uninstall: delete SKILL.md from every target

# Everything, scripted.
memory setup all --yes
```

`memory setup rules` writes a `<memory-rules>…</memory-rules>` block (loose-XML markers so it is easy to locate and update) and saves a `.bak` sibling before each modification. Re-running replaces the block in place — your agent rule files never accumulate duplicates. If the companion [`agent-tools setup rules`](https://github.com/nitecon/agent-tools) block (`<agent-tools-rules>…</agent-tools-rules>`) is already present in the file, the memory block is inserted directly after it so the two protocols stay grouped at the top; otherwise it is prepended.

**Interaction with native agent memory.** The installed rules block directs the agent to route every memory operation through the `memory` CLI, so leaving each tool's built-in memory system enabled would cause the agent to write into both the tool's native memory surface and this tool's SQLite store simultaneously — silent duplication the rules block is specifically designed to avoid. For every supported frontend, `memory setup rules` also merges the matching native-memory-disable setting; `--remove` reverses each merge by deleting (not forcing) the key so prior state is restored.

| Agent  | Target file                 | Change on install                                         |
|--------|-----------------------------|-----------------------------------------------------------|
| Claude | `~/.claude/settings.json`   | set `"autoMemoryEnabled": false` (also `./.claude/settings.json` for project-scope `CLAUDE.md`) |
| Gemini | `~/.gemini/settings.json`   | append `"save_memory"` to `excludeTools` (disables Gemini's built-in memory tool) |
| Codex  | `$CODEX_HOME/config.toml` (or `~/.codex/`, then `~/.config/codex/`) | set `[features] memories = false` (disables the Chronicle memory feature) |

All three merges are conservative: unrelated keys, tables, and array entries are preserved; corrupt input fails loudly instead of being overwritten; re-runs are no-ops once the target state is reached. Writes are atomic (`.new` + rename) so a crash mid-write cannot leave a half-serialized file behind.

`memory setup skill` writes the same SKILL.md byte-for-byte to every known agent frontend — `~/.claude/skills/agent-memory/SKILL.md` (Claude Code, tool-native) and `~/.agents/skills/agent-memory/SKILL.md` (the cross-agent alias read by both Gemini CLI and Codex). All frontends honor the same YAML frontmatter + Markdown body; Gemini and Codex silently ignore the Claude-specific `allowed-tools` key. The frontmatter `description` is always loaded into sessions (~100 tokens), pulling the model toward `memory context` at task start and `memory store` at task end. The full body only loads on demand when the skill is picked. Install is unconditional — no auto-detection of whether each agent is installed — because running `memory setup skill` is itself the opt-in signal. Re-runs write a `.bak` sidecar then overwrite, so the command is idempotent. The legacy `~/.gemini/skills/agent-memory/SKILL.md` path (written by v1.4.0/v1.4.1) is scrubbed on every install and `--remove` so no stale copy lingers.

### Manual install (or inspect the live block)

The exact `<memory-rules>` block evolves with the CLI surface, so this README no longer inlines a copy that can drift out of date. To see — or hand-paste — the current block, print it straight from the binary:

```bash
memory setup rules --print     # emit the live <memory-rules> block to stdout
```

Add that output to your global `CLAUDE.md`, `GEMINI.md`, or equivalent agent instructions. Prefer `memory setup rules` over hand-pasting — it injects the same block between locatable markers and re-runs replace it in place, so your rule files never accumulate duplicate or stale copies.

## MCP server (optional)

If you prefer MCP integration, register the server:

```bash
claude mcp add agent-memory -- /opt/agentic/bin/memory serve
```

Or add manually to `~/.claude.json`:

```json
{
  "mcpServers": {
    "agent-memory": {
      "type": "stdio",
      "command": "/opt/agentic/bin/memory",
      "args": ["serve"]
    }
  }
}
```

This gives Claude Code thirteen native tools: `memory_store`, `memory_search`, `memory_recall`, `memory_forget`, `memory_prune`, `memory_context`, `memory_get`, `memory_projects`, `memory_move`, `memory_copy`, `memory_working_get`, `memory_working_set`, `memory_working_clear`.

### Skills (optional)

Copy the skill directories to your Claude Code skills location:

```bash
# Personal skills (available in all projects)
cp -r skills/remember ~/.claude/skills/remember
cp -r skills/recall ~/.claude/skills/recall
```

This enables `/remember` and `/recall` slash commands.

## CLI reference

```bash
# Store a memory (project auto-detected from cwd's git remote)
memory store "User prefers terse responses" --tags "preference" -m feedback

# Store a universal preference (applies across every repo, 1.25× retrieval boost)
memory store "User never wants PRs opened unless explicitly asked" \
  -m feedback --scope global --tags "workflow,pr"

# Hybrid search (BM25 + vector); cwd project is boosted
memory search "how does testing work"

# Fetch full content for specific hits (two-stage retrieval)
# Short 8-char prefix is fine — resolves via `resolve_id_prefix`.
memory get 4c82c482
memory get <uuid> <uuid>

# Filter by project/agent/tags
memory recall --project myapp --memory-type feedback

# Task-relevant context
memory context "refactoring the auth middleware" -k 5

# Hard filter vs boost
memory search "storage" --only "github.com/acme/infra.git"   # only this project
memory search "storage" --no-project-boost                    # flat ranking, no boost

# Delete by ID (short prefix supported) or by search
memory forget --id 4c82c482
memory forget --query "outdated preference"

# Clean up stale memories
memory prune --max-age-days 90 --dry-run
memory prune --max-age-days 90

# List all memories
memory list -k 50 --project myapp

# List distinct project idents (great for spotting alias mismatches)
memory projects

# Exchange durable project memories with the agent gateway
memory push status
memory push
memory pull status
memory pull

# Migrate memories from a legacy project name to the canonical git-remote ident
memory move --from "trading-platform-sre" --to "github.com/nitecon/SRE.git" --dry-run
memory move --from "trading-platform-sre" --to "github.com/nitecon/SRE.git"

# Reassign a single memory by ID (pass --to "" to clear the project tag)
memory move --id <uuid> --to "github.com/nitecon/SRE.git"
memory move --id <uuid> --to ""

# Duplicate memories under a new project ident (preserves content + embedding)
memory copy --from "github.com/acme/mono.git" --to "github.com/acme/split.git"
memory copy --id <uuid> --to "github.com/acme/mirror.git"

# Check for updates and install the latest version
memory update

# Setup family — interactive checklist + per-component subcommands
memory setup                              # interactive: pick rules and/or skill
memory setup rules                        # rules only: detect + prompt
memory setup rules --all                  # rules: update every detected file
memory setup rules --target ~/.claude/CLAUDE.md
memory setup rules --dry-run              # rules: preview, don't write
memory setup rules --print                # rules: print <memory-rules> block
memory setup rules --all --remove         # rules: strip block + reverse every native-memory-disable write
memory setup skill                        # install SKILL.md under Claude + cross-agent (~/.agents) skill dirs
memory setup skill --dry-run              # skill: preview SKILL.md
memory setup skill --print                # skill: print SKILL.md to stdout
memory setup skill --remove               # skill: delete SKILL.md from every target
memory setup all --yes                    # gateway → rules → skill, non-interactive
```

## Project & global scope tiers

`store`, `search`, and `context` derive the current project identifier from the working directory's git remote and reduce it to the repository shortname (e.g. `git@github.com:nitecon/eventic.git` → `eventic`). SSH and HTTPS for the same repo produce the same ident. Non-git directories fall back to the directory basename. New memories are auto-tagged with this project unless you pass `--project` explicitly, `--no-project`, or `--scope global`.

Shortname is deliberate so auto-derived idents match the hand-written shortnames most agents already use. The trade-off is that two repos with the same basename across different orgs will collide; in that case, tag them explicitly with `--project`.

Retrieval applies two independent score boosts:

| Scope | Boost | Meaning |
|-------|-------|---------|
| Current project (`project == cwd`) | **1.5×** | Local context — highest priority |
| Global (`project == "__global__"`) | **1.25×** | Universal user preferences — surface in every repo |
| Other project | 1.0× | Cross-project prior art; flagged via the `hint` field |

A single `context` or `search` call returns hits from all three tiers; the response's `cross_project_count`, `global_scope_count`, and `hint` fields tell models how to weigh them. Strong cross-project matches can still out-rank weak current-project hits — the boosts tilt ties without hard-filtering prior art.

### Global scope

Global-scoped memories are stored under the reserved sentinel project ident `__global__`. Users opt in with `--scope global` on `memory store`:

```bash
memory store "Never open a PR unless explicitly asked" \
  -m feedback --scope global --tags "workflow,pr"
```

The sentinel is reserved: passing `--project __global__` directly (or `memory move --to __global__`) is rejected with a clear error pointing users to `--scope global`. This keeps the sentinel load-bearing for retrieval behavior rather than a string users can accidentally collide into. When you run `memory projects`, `__global__` shows up as its own row so you can see how many universal preferences are on file.

### Search flags

| Flag | Behavior |
|------|----------|
| (none) | Boost cwd project (1.5×) **and** global sentinel (1.25×); cross-project results still surface |
| `-p <ident>` | Boost this project (1.5×) instead of cwd; global boost unchanged |
| `--only <ident>` | Hard filter: only return memories with this project |
| `--no-project-boost` | Flat ranking; disables **both** boosts |

### Store flags

| Flag | Behavior |
|------|----------|
| (none) | Project scope; `project` auto-detected from cwd |
| `--scope project` | Explicit project scope; suppresses the reflection hint even on `user`/`feedback` stores |
| `--scope global` | Global scope; stores under the `__global__` sentinel |
| `--project <ident>` | Override the project ident (must NOT equal `__global__`) |
| `--no-project` | Store with no project tag (skips cwd auto-detect) |

## Migrating project idents

If memories were stored under a legacy project name (e.g. a logical label like `trading-platform-sre`) but the cwd-resolver now returns the canonical git-remote ident (e.g. `github.com/nitecon/SRE.git`), search will treat them as cross-project and the `hint` field will undersell their relevance. Fix it by consolidating idents:

```bash
# 1. Inspect the distinct project idents in the database
memory projects

# 2. Preview the affected memories before writing
memory move --from "trading-platform-sre" --to "github.com/nitecon/SRE.git" --dry-run

# 3. Apply the rename
memory move --from "trading-platform-sre" --to "github.com/nitecon/SRE.git"
```

Use `memory copy` instead of `memory move` when you want the memory available under *both* idents — for example, when a shared memory applies to two forks of the same codebase. Copies keep the original content, tags, and cached embedding; only the project ident, UUID, and timestamps differ.

## Output format (light-XML)

All commands emit **light-XML** — grouped section tags with numbered content lines. No JSON. The shape is compact on purpose: tags give the agent a structural signal while the payload stays plain lines so token overhead is minimal.

Content bodies are **not** entity-escaped. Angle brackets, ampersands, and quotes pass through raw so guidance text like `` `memory get <id>` `` renders readably instead of as `&lt;id&gt;`. The only escape is `"` → `&quot;` inside attribute values (needed so the `"..."` delimiter isn't broken).

### `context` / `search` / `recall`

```
<project_memories>
1. PRs required by CodingGuidelines.md [git,standards] (ID:4c82c482)
2. Follow docs/CodingGuidelines.md for PRs [git,standards] (ID:772fd580)
</project_memories>
<general_knowledge>
1. User avoids PRs unless required [git,standards] (ID:372bd79d)
</general_knowledge>
<other_projects>
1. colorithmic: k-means Euclidean beats OKLab on OPT [quantization] (ID:23d0142a)
</other_projects>
<hint>2 of 4 results are global-scope preferences (apply across all projects). Treat them as directives, not suggestions.</hint>
<usage>IDs are 8-char prefixes. Use `memory get <id>` for full content. Sections: project_memories=current repo, general_knowledge=user-wide directives, other_projects=prior art.</usage>
```

- `<project_memories>` — hits tagged with the current (cwd-derived) project.
- `<general_knowledge>` — hits tagged with the `__global__` sentinel (universal preferences, 1.25× boost).
- `<other_projects>` — hits from other projects, prefixed with the originating project ident. Treat as prior art.
- `<hint>` — reflection / directive prompt. Only emitted when it has something to say.
- `<usage>` — static legend documenting short-ID semantics and section meanings. Emitted unconditionally on every multi-memory read (`context`, `search`, `recall`, `list`) — including zero-result runs — so cold callers always have the key in reach. Positioned at the bottom so structured data comes first.

Empty sections are elided. A query with zero global-scope hits during a scoped retrieval triggers a reflection-style `<hint>` nudging the agent to confirm no universal preference applies before acting.

### Mutations (`store` / `move` / `copy` / `forget` / `prune`)

Single self-closing `<result>` line:

```
<result status="stored" id="a4936eff" scope="global" project="__global__"/>
<result status="forgot" id="a4936eff"/>
<result status="forgot" count="3"/>
<result status="no_matches"/>
<result status="pruned" count="7"/>
<result status="dry_run" count="7"/>
<result status="moved" id="a4936eff" to_project="github.com/acme/split.git"/>
```

`memory store` with memory type `user` or `feedback` and no explicit `--scope` gets one additional `<hint>…</hint>` line reminding the caller to reclassify to global if the memory applies across repos.

### `memory get`

```
<memory id="a4936eff" project="agent-memory" type="feedback" tags="workflow,pr">
User never wants PRs opened unless they explicitly ask.
</memory>
```

Full content is emitted verbatim as element text (XML-escaped). IDs are shown as 8-char short prefixes everywhere — full UUIDs still work as input.

### Short-ID resolution

Every command that takes an `<id>` accepts any prefix of 4 or more hex characters. `memory get 4c82c482` expands to the full UUID when unique. When two memories share the same prefix, an `<ambiguous>` block lists the candidates:

```
<ambiguous prefix="4c82c482">
1. 4c82c482-c081-4937... [colorithmic,milestone]: colorithmic v1.0.0 milestone 2026-04-20...
2. 4c82c482-d7f2-4a18... [agent-memory,schema]: Schema v3 migration design notes...
Reply with 1..2, or re-run with a longer prefix.
</ambiguous>
```

The fast-path full-UUID lookup still works — prefix resolution is additive.

### `memory list` / `memory projects`

Plain light-XML blocks optimized for readability:

```
<memories count="2">
1.*(feedback) agent-memory [workflow,pr] (ID:a4936eff): User never wants PRs opened unless they explicitly ask.
2. (user) colorithmic [setup] (ID:b12c3d4e): Prefer k-means Euclidean over OKLab for OPT quantization.
</memories>
<usage>IDs are 8-char prefixes. Use `memory get <id>` for full content. Sections: project_memories=current repo, general_knowledge=user-wide directives, other_projects=prior art.</usage>
```

```
<projects count="3">
*agent-memory (42)
 colorithmic (7)
 __global__ (3)
</projects>
```

A leading `*` marks the current cwd-derived project. An empty list collapses to a self-closing `<memories count="0"/>` or `<projects count="0"/>`. `memory list` also emits the `<usage>` legend (it's a multi-memory read); `memory projects` does not (it's a utility listing of idents, not a memory read).

## Auto-update

The binary checks for new releases on GitHub once per hour (at most) during normal CLI usage. If a newer version is found, it downloads and replaces the binary automatically. The update check is non-blocking — failures are logged to stderr and never interrupt normal operation.

To disable auto-updates, set the environment variable:

```bash
export AGENT_MEMORY_NO_UPDATE=1
```

You can also trigger an update manually at any time with `memory update`.

`memory update` fetches the combined release archive and atomically swaps **both** binaries (`memory` + `memory-dream`) in place. If `memory-dream` wasn't previously installed, the updater force-bundles it on the next upgrade — users who never run the compactor pay ~28MB of disk but no cognitive overhead.

## Dream compactor (offline condensation + dedup)

`memory-dream` is a one-shot batch utility that walks your memory DB. Each pass runs three stages in order:

1. **Stage 0 — project review** (the primary cross-memory consolidation path). Every memory in a project is sent to the model as a single message and the model returns a structured decision per memory: `keep`, `drop` (e.g. reconstructable from `git log`), `merge_into:<id>` (fold this memory's facts into another), `supersede_by:{content, tags?}` (replace a cluster with one canonical entry), or `extract:{content, tags?}` (drop the framing, retain a buried note as a new memory). Because it sees the whole project at once, it catches paraphrased duplicates that share no vocabulary.
2. **Stage A — cosine dedup** over the Stage 0 survivors. Near-identical memories are matched by cosine similarity on the embeddings; the older of a duplicate pair gets a `superseded_by` pointer to the newer one. Default reads filter superseded rows out, so they stay in the DB for audit but never surface in search / context / list. This is a cheap secondary signal that catches byte-identical inserts without a model round-trip.
3. **Stage B — per-memory condense**. Surviving verbose memories are condensed into a shorter factual claim using an in-process gemma3 model (via `candle`). The original text is preserved in a new `content_raw` column so nothing is lost — condensed text replaces `content`, raw text lives alongside.

It's never a daemon. Each invocation loads the model, processes the DB, and exits. Run it however you like: cron, `launchd`, Windows Task Scheduler, or just manually after a heavy session.

### First-time setup

The default model `gemma3` (`google/gemma-3-1b-it`) is **gated** on HuggingFace — you must accept its license and supply an access token before `--pull` will succeed.

```bash
# 1. Visit https://huggingface.co/google/gemma-3-1b-it and accept the license.
# 2. Create an access token at https://huggingface.co/settings/tokens.
# 3. Export the token (HF_TOKEN or HUGGING_FACE_HUB_TOKEN both work).
export HF_TOKEN=hf_xxx_your_token_xxx

# 4. Download. ~2GB, cached under $AGENT_MEMORY_DIR/models/gemma3/.
#    Resume-safe: interrupt with Ctrl-C and re-run to continue from the
#    same byte offset. Idempotent: subsequent runs with all files present
#    exit with `<result status="pull_skipped"/>` and no network activity.
memory-dream --pull
```

`--pull` verifies the SHA-256 of every downloaded file against the hash HuggingFace advertises (the LFS-pointer digest). On a checksum mismatch — including a previously-cached file that fails verification on a later run — the corrupt file is deleted and re-downloaded. When HuggingFace advertises no usable hash for a file, that file emits `<result status="checksum_skipped"/>` and the pull continues.

Without a token, `memory-dream --pull` emits `<result status="auth_required" .../>` and the three-step remediation above, then exits non-zero.

### Smoke-testing the pull pipeline (no auth required)

For CI and contributors without an HF token, two short-names resolve to ungated repos so the download plumbing can be exercised end-to-end without credentials:

- `smollm` → `HuggingFaceTB/SmolLM-135M-Instruct` (~300MB) — the lightweight laptop-CPU / CI smoke model.
- `tinyllama` → `TinyLlama/TinyLlama-1.1B-Chat-v1.0` (~2GB) — a higher-quality smoke model when 135M is too small.

```bash
export AGENT_MEMORY_DIR=/tmp/dream-smoke
memory-dream --pull --model smollm     # or: --model tinyllama
```

Neither is wired into the condenser — they exist solely to validate the pull flow. Real condensation still requires `gemma3`.

### Regular use

```bash
# Preview: walk the DB and report what would change. No writes.
memory-dream --dry-run

# Apply: full pass. Per-memory BEGIN IMMEDIATE transactions serialize
# with any concurrent `memory store` writes.
memory-dream

# Cap the pass for incremental runs on large DBs.
memory-dream --limit 50

# Re-evaluate every memory, ignoring the "recently dreamed" cutoff and the
# condenser_version freshness check — use after a prompt or model change.
# `--full` is the equivalent older spelling of the same switch.
memory-dream --refresh

# Swap model (rare — any HF repo id works; short names `gemma3`, `smollm`,
# and `tinyllama` resolve to canonical repos, everything else passes through
# unchanged).
memory-dream --model myorg/my-fork
```

The bare invocation (above) is equivalent to `memory-dream run`. The same global flags — `--model`, `--dry-run`, `--limit`, `--full` / `--refresh`, `--batch-size`, `--backend`, `--command-override` — apply to the bare pass and to the `test` subcommand; they override settings for a single invocation and never mutate `dream.toml`.

### Subcommands

`memory-dream` also exposes a small subcommand family for managing its backend and inspecting configuration. All write to (or read from) the `dream.toml` settings file unless noted.

| Command | Behavior |
|---------|----------|
| `memory-dream run` | Explicit alias for the bare invocation (run a dream pass). |
| `memory-dream config show` | Dump `dream.toml` as light-XML. |
| `memory-dream config set <key> <value>` | Mutate a dotted key, e.g. `config set backend.mode headless` or `config set headless.timeout_ms 60000`. |
| `memory-dream use <model>` | Set `local.active_model` to a short-name **and** flip `backend.mode` to `local`. |
| `memory-dream use --headless` | Flip the backend to `headless` (run an external CLI via the `headless.command` template). |
| `memory-dream use --disabled` | Flip the backend to `disabled` (dedup-only pass — Stage B condense is skipped). |
| `memory-dream rm <model>` | Delete a local model's cache directory and drop it from `downloaded_models` in `dream.toml`. |
| `memory-dream list` | Dump the effective settings (backend, active model, downloaded models, PATH-detected CLIs). |
| `memory-dream test <id>` | Preview condensation for a single memory without writing to the DB; honors the override flags so a memory can be A/B'd across backends. |

`backend.mode` is one of `local` (in-process candle model), `headless` (shell out to an external command template), or `disabled` (dedup only, no condense).

### Scheduling examples

**cron (Linux/macOS)** — daily at 03:00 local time:

```cron
0 3 * * * /opt/agentic/bin/memory-dream >> ~/.agentic/dream.log 2>&1
```

**launchd (macOS)** — run after login, again daily:

```xml
<!-- ~/Library/LaunchAgents/com.agentic.dream.plist -->
<plist version="1.0"><dict>
  <key>Label</key><string>com.agentic.dream</string>
  <key>ProgramArguments</key>
  <array>
    <string>/opt/agentic/bin/memory-dream</string>
  </array>
  <key>StartInterval</key><integer>86400</integer>
  <key>RunAtLoad</key><true/>
</dict></plist>
```

**Windows Task Scheduler** — daily at 03:00:

```powershell
$action = New-ScheduledTaskAction -Execute "$env:USERPROFILE\.agentic\bin\memory-dream.exe"
$trigger = New-ScheduledTaskTrigger -Daily -At 3am
Register-ScheduledTask -TaskName "agent-memory-dream" -Action $action -Trigger $trigger
```

### What gets condensed, what gets deduped

A memory needs condensation when it has `content_raw IS NULL` (never processed) OR its `condenser_version` stamp no longer matches the current `<model>:<prompt-hash>` combo (prompt or model changed since last run). Stamping lets future passes detect and re-run stale rows without reprocessing everything.

Dedup candidates must share the same `project` AND same `memory_type` AND same `embedding_model`. The cosine threshold defaults to `0.87` (empirically tuned for `all-MiniLM-L6-v2`). On match, the row with the earlier `created_at` is marked superseded. An exact-match short-circuit runs before the cosine scan so byte-identical inserts don't pay the vector cost.

### Safety nets

- **Prompt injection defense**: the condensation prompt wraps memory content in `<<<MEMORY>>> ... <<<END>>>` and explicitly instructs the model to treat anything inside as data, not instructions. A single few-shot example anchors verbatim preservation of paths / numbers / dates. The response must be JSON (`{"condensed": "..."}`); non-JSON triggers a fallback to the raw memory.
- **Length-ratio check**: if the model's "condensed" output is longer than the input, it's rejected and the raw memory stays untouched.
- **Refusal detection**: responses matching `I cannot`, `I'm sorry, but`, `as a language model`, etc. fall back to the raw memory.
- **Per-memory error containment**: one bad memory can't halt the pass. Errors are logged and the orchestrator moves on.
- **`--dry-run` writes nothing**: row counts are identical before/after a dry-run pass.
- **BEGIN IMMEDIATE transactions**: every mutation runs inside a per-memory immediate transaction so concurrent `memory store` calls can't race.

## MCP tools

| Tool | Purpose |
|------|---------|
| `memory_store` | Save memory with auto-embedding + BM25 indexing |
| `memory_search` | Hybrid BM25 + vector search, returns ranked results |
| `memory_recall` | Filter by project/agent/tags/type |
| `memory_forget` | Remove specific memories |
| `memory_prune` | Decay stale/low-access memories |
| `memory_context` | Return top-K relevant memories for a task description |
| `memory_get` | Fetch full content for one or more memory IDs (full UUID or 4+ char short prefix) |
| `memory_projects` | List distinct project idents with memory counts (spot alias mismatches) |
| `memory_move` | Reassign the project ident on one memory (by id) or in bulk (by from/to) |
| `memory_copy` | Duplicate memories under a new project ident; preserves content + embedding |
| `memory_working_get` | Return the current project's WorkingContext handoff (`present="false"` when none) |
| `memory_working_set` | Replace the current project's WorkingContext handoff (65,536-char cap) |
| `memory_working_clear` | Delete the current project's WorkingContext handoff (idempotent) |

## Memory types

| Type | Purpose |
|------|---------|
| `user` | Facts about the user -- role, preferences, expertise |
| `feedback` | How to approach work -- corrections and confirmed approaches |
| `project` | Ongoing work context -- decisions, deadlines, constraints |
| `reference` | Pointers to external resources -- URLs, dashboards, systems |

## How search works

Every query runs through two retrieval paths simultaneously:

1. **BM25** (FTS5) -- term-frequency keyword matching, great for exact names and patterns
2. **Vector** (fastembed cosine similarity) -- semantic similarity, great for "I vaguely remember something about..."

Results are combined via **Reciprocal Rank Fusion** (k=60), which merges ranked lists without requiring score normalization. A memory that ranks well in both paths gets a strong combined score.

## Design decisions

- **SQLite is the source of truth.** FTS5 handles full-text indexing within the same database file.
- **Embeddings are brute-force cosine.** For a personal memory system (<100K memories), this is fast enough and avoids ANN index complexity.
- **Model loads lazily.** Commands that don't need embeddings (e.g., `recall`, `forget --id`) skip the ~200ms model load.
- **Access counts track usage.** Every retrieval increments `access_count`, enabling `prune` to identify stale memories.
- **All logging goes to stderr.** Stdout is reserved for light-XML results (CLI) or JSON-RPC transport (MCP), so logging never pollutes either channel. MCP tool responses themselves are light-XML strings delivered as a single text content block.
- **User-writable default storage.** Fresh installs store data under `~/.agentic/` to avoid `/opt` permission failures; existing writable `/opt/agentic/memory.db` installs are preserved as legacy shared databases.
