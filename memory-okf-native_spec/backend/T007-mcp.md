# T007 - Expose OKF-native MCP tools and resources

**Depends on:** T004, T005

## Scope

Expose OKF concept reads, validation, bundle navigation, history, diff, graph,
and explicit revision-aware put through MCP. Add virtual `okf+memory://`
resources where supported by the MCP library.

## Acceptance criteria

- [ ] MCP read tools match CLI handler semantics and return bounded content.
- [ ] Put is explicitly described as mutating durable memory and supports expected revision.
- [ ] MCP and CLI use the same service/handler code, not parallel implementations.
- [ ] Project/global/unscoped authorization and isolation match existing memory scope rules.
- [ ] WorkingContext is never exposed as an OKF resource.
- [ ] MCP schema and serialization tests cover every new input/output.
- [ ] Existing MCP tools remain backward compatible.

## Touch surface

- `memory/src/mcp.rs`
- `memory/src/okf/`
- MCP tests
