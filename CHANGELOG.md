# Changelog

## v1.7.0 - 2026-05-18

- Added per-project WorkingContext handoff state with `memory working get`, `memory working set`, and `memory working clear`.
- `memory context` and `memory_context` now render active WorkingContext before ranked memories without consuming the search result budget.
- Added MCP tools `memory_working_get`, `memory_working_set`, and `memory_working_clear`.
- Added `.agent/api/memory.yaml` API context documentation for the memory CLI and MCP surfaces.
- Project-wide `memory move` and `memory_move` now transfer or clear WorkingContext; `memory copy` and `memory projects` remain durable-memory-only.
