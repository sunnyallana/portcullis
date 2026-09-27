# Changelog

## 0.2.0 — 2026-09-27

Per-request identity, a second database, and the tooling that makes a
deployment survivable.

### Added
- **HTTP transport.** MCP over HTTP with identity resolved per request, so one
  process serves many callers with different scopes. Both transports share one
  dispatch, so stdio and HTTP cannot answer differently.
- **OIDC authentication.** Tokens validated for signature, issuer, audience and
  expiry, with the algorithm taken from the key rather than the token header.
  Keys load at startup and refresh on rotation. Only claims the operator maps
  become caller attributes.
- **API keys**, stored as digests and compared in constant time, plus
  `portcullis apikey` to mint one.
- **Approval controls**: `approver_roles`, and self-approval refused by
  default.
- **Approvals JSON API and console**, with a per-viewer `decidable` flag.
- **MySQL backend**, including a `RETURNING` emulation that re-selects the
  written row on the same connection.
- **`portcullis profile`**: reads a database, samples rows, classifies columns
  (Luhn-checked cards, email shapes, credential-looking names) and drafts a
  configuration with masks filled in. Reads only; no sampled value is printed
  in the clear.
- **`portcullis replay`**: re-runs recorded read calls and diffs them against a
  baseline, exiting 3 on a difference. Writes are never replayed.
- `/healthz`, `/readyz` and Prometheus `/metrics`; graceful shutdown.
- The audit record now carries the caller's scope.

### Changed
- `Engine::approve` and `Engine::deny` take a `Caller` rather than a name, so
  the approver's role is checked.
- `portcullis_sql::write` returns the resolved column values alongside the
  statement, which is what lets a backend without `RETURNING` find the row.

### Known limitations
- No versioned action bundles; a configuration change is an edit and a restart.
- Rate limits and the idempotency store are per process.
- Audit rotation is manual and needs the process stopped.

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
- Hash-chained append-only audit log, with `portcullis audit verify` detecting an
  edited or deleted record.
- PostgreSQL backend over a pooled, TLS-capable connection, verified against a
  live PostgreSQL 18 server: catalogue introspection, value binding and
  decoding, row-filter enforcement on reads, inserts and updates, and server
  error reporting with the SQLSTATE carried through.
- In-memory backend for the demo and the test suite.
- MCP server over stdio with concurrent request handling.
- `portcullis` CLI: `init`, `validate`, `doctor`, `tools`, `serve`, `call`,
  `approvals`, `audit`.

### Known limitations
- One role per process; HTTP transport with OIDC is not built yet.
- Rate limits and the idempotency store are per process.
- Audit rotation must be done with the process stopped.
