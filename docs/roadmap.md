# Roadmap

What exists, what is next, and why in that order. Dated 2026-09-28.

## Built

- Declarative actions validated against the live schema at startup
- PostgreSQL and MySQL backends, pooled, TLS via rustls, both with integration
  tests against real servers
- Reads with caller-scoped row filters, masking, ordering, row ceilings and
  per-action timeouts
- Writes (insert, upsert, update) with row-filter enforcement, returning
  columns — emulated on MySQL, which has no `RETURNING` — and replay protection
- Approval gates: durable queue, `approver_roles`, no self-approval, one claim
  per request
- Per-caller, per-action rate limiting
- Hash-chained append-only audit log with tamper detection, carrying the
  caller's scope
- MCP over stdio and over HTTP, sharing one dispatch
- OIDC and API-key authentication, with identity resolved per request
- Approvals JSON API and a small console; `/healthz`, `/readyz`, `/metrics`
- `portcullis profile`: read a database, classify columns, draft a configuration
- `portcullis replay`: re-run recorded reads and diff against a baseline
- Versioned action bundles: several versions served at once, aliases, sticky
  per-caller canary, every version validated at startup, the version recorded
  on every audit line
- `approval_status`, so a caller can find out what became of a write it parked
- Rate limits and replay protection shareable across replicas, in the database
- Audit rotation into sealed segments, with the chain running through them
- `init`, `validate`, `doctor`, `tools`, `bundles`, `call`, `serve`,
  `approvals`, `audit`, `replay`, `apikey`

## Next, in order

**1. SQL Server.** The only backend left that can be done properly here, and
it is more work than MySQL was. `sqlx` dropped MSSQL support after 0.6, so
this is not another `sqlx` feature flag: it needs `tiberius` with its own
pool, its own type mapping, `@P1` placeholders, `[bracket]` quoting,
`OFFSET/FETCH` instead of `LIMIT`, and `OUTPUT INSERTED.*` instead of
`RETURNING`. Testable against `mcr.microsoft.com/mssql/server`, which is the
part that matters.

**2. Snowflake and BigQuery.** Deliberately not started. Both are REST APIs
rather than wire-protocol drivers, and neither has a local emulator worth
testing against, so writing them now would mean shipping two backends that
have never executed a query — against a codebase where every other backend
has live integration tests. They need a real account and a real dataset
before the first line, and cost controls matter more than row limits there,
which is a design question of its own.

**3. Telling the agent sooner.** `approval_status` closes the gap by letting a
caller poll. A webhook or a long poll would close it better for unattended
runs that would rather not spin.

**4. Audit shipping.** Rotation seals segments; nothing yet pushes them to
object storage or records their head digests anywhere external. The seal makes
that useful, so it is the natural next step.

## Considered and deliberately not done

- **Free-form SQL with a policy check.** Parsing arbitrary SQL and deciding
  whether it is safe is a much harder problem than declaring what is allowed,
  and the failure mode is silent.
- **Its own identity provider.** Portcullis should consume identity, never own it.
- **Response caching.** Stale data in an agent's context is worse than a slow
  query.
- **Replaying writes.** Asked for more than once. Re-running a refund to see
  whether it still works is not a test, it is a second refund.

## Known limitations

- Rate limits and replay protection are per process unless
  `[limits] store = "database"`, and the shared window is a fixed minute
  rather than a sliding one.
- Archiving sealed segments off the host is still manual.
- An agent has to poll `approval_status`; nothing calls it back.
- `/metrics` is unauthenticated by design; keep it off public interfaces.
- MySQL: no UUID type (use `CHAR(36)`), and `returning` costs a second round
  trip.
- The in-memory backend has no transactions and is for demos and tests only.
- No external security audit. The controls are tested, not certified.
