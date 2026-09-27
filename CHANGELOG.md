# Changelog

## 0.1.0 — 2026-09-27

First working version.

### Added
- Declarative actions in TOML, validated against the live database schema at
  startup: tables, columns, parameter types, masks, keys and role grants.
- A small predicate language for `filter` and `row_filter` that cannot express
  a function call, a subquery or a second table.
- Reads with caller-scoped row filters, column masking, ordering, row ceilings
  and per-action timeouts.
- Writes (insert, upsert, update) with row-filter enforcement, returning
  columns, and idempotency keys so a retried call does not write twice.
- Approval gates with a durable queue and single-claim release.
- Per-caller, per-action rate limiting.
- Hash-chained append-only audit log, with `sluice audit verify` detecting an
  edited or deleted record.
- PostgreSQL backend over a pooled, TLS-capable connection.
- In-memory backend for the demo and the test suite.
- MCP server over stdio with concurrent request handling.
- `sluice` CLI: `init`, `validate`, `doctor`, `tools`, `serve`, `call`,
  `approvals`, `audit`.

### Known limitations
- One role per process; HTTP transport with OIDC is not built yet.
- Rate limits and the idempotency store are per process.
- Audit rotation must be done with the process stopped.
