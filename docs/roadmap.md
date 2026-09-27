# Roadmap

What exists, what is next, and why in that order. Dated 2026-09-27.

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
- `sluice profile`: read a database, classify columns, draft a configuration
- `sluice replay`: re-run recorded reads and diff against a baseline
- `init`, `validate`, `doctor`, `tools`, `call`, `serve`, `approvals`, `audit`,
  `apikey`

## Next, in order

**1. Versioned action bundles.** The one substantial thing still missing. Today
a configuration change is a file edit and a restart, which is fine for one
deployment and awkward for a fleet: there is no way to canary `orders@v4`
against ten percent of traffic, and no way to roll back without another edit.
The shape is a named, versioned set of actions with aliases, resolved per
request, with the registry keyed by version. `sluice replay` already provides
the safety net this would promote against; it is the promotion mechanism that
is missing.

**2. Shared limits.** Rate limiting and the idempotency store are per process,
so a horizontally scaled deployment enforces them per replica. Both want to
move into the database behind a small trait, which also makes replay protection
survive a restart of a different pod.

**3. More backends.** SQL Server next, then Snowflake and BigQuery where cost
controls matter more than row limits. `Dialect` and `Backend` are both already
the seam; MySQL took a day and proved it.

**4. Audit log rotation and shipping.** Rotation is manual and must be done
with the process stopped. It should rotate on size or age, seal each segment
with its head digest, and optionally push segments to object storage.

**5. Multi-step actions.** One action that is really a join, a lookup and a
write, with pushdown where the backend can do the work. Useful, and not before
everything above is solid.

## Considered and deliberately not done

- **Free-form SQL with a policy check.** Parsing arbitrary SQL and deciding
  whether it is safe is a much harder problem than declaring what is allowed,
  and the failure mode is silent.
- **Its own identity provider.** Sluice should consume identity, never own it.
- **Response caching.** Stale data in an agent's context is worse than a slow
  query.
- **Replaying writes.** Asked for more than once. Re-running a refund to see
  whether it still works is not a test, it is a second refund.

## Known limitations

- No versioned bundles, so configuration changes are edit-and-restart.
- Rate limits and idempotency are per process.
- Audit rotation is manual and needs the process stopped.
- `/metrics` is unauthenticated by design; keep it off public interfaces.
- MySQL: no UUID type (use `CHAR(36)`), and `returning` costs a second round
  trip.
- The in-memory backend has no transactions and is for demos and tests only.
- No external security audit. The controls are tested, not certified.
