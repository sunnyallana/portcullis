# Roadmap

What exists, what is next, and why in that order. Dated 2026-09-27.

## In v0.1

- Declarative actions validated against the live schema at startup
- PostgreSQL backend, pooled, TLS via rustls
- Reads with row filters, masking, ordering, row ceilings, timeouts
- Writes: insert, upsert, update, with row-filter enforcement and replay protection
- Approval gates, durable queue, single-claim release
- Per-caller, per-action rate limiting
- Hash-chained append-only audit log with tamper detection
- MCP over stdio, concurrent request handling
- `init`, `validate`, `doctor`, `tools`, `call`, `serve`, `approvals`, `audit`

## Next, in order

**1. HTTP transport with OIDC.** The largest real gap. Today one process serves
one role; a shared deployment needs the caller's token to supply the role and
its attributes. Everything downstream of `Caller` is already built for it —
`Engine::call` takes a caller per request. Needs: streamable HTTP per the MCP
spec, token validation, claim-to-attribute mapping, and a session story.

**2. A schema profiler that drafts the config.** Writing actions by hand for a
200-table database is the thing most likely to stop an evaluation. Read the
schema and a data sample, infer candidate actions from primary and foreign
keys, and classify columns that look like email addresses, card numbers or
national IDs so masks are pre-filled. Output a draft TOML for a human to edit.
This is the demo that sells the product.

**3. More backends.** MySQL and SQL Server next; the `Dialect` trait and the
`Backend` trait are both already the seam. Snowflake and BigQuery after, where
cost controls matter more than row limits.

**4. Action bundles and versioning.** Named, versioned sets of actions with
aliases, so `orders@v4` can be canaried and rolled back without editing a file
in production.

**5. An approvals console.** A small web view of the queue with a diff of what
the write will do. The CLI and the JSONL file are fine for a handful of
approvals a day and not fine for fifty.

**6. Recorded replay for regression testing.** Capture real calls, replay them
after a config change, show what differs. Sells separately as evidence for a
change-advisory board.

## Considered and deliberately not done yet

- **Free-form SQL with a policy check.** Parsing arbitrary SQL and deciding if
  it is safe is a much harder problem than declaring what is allowed, and the
  failure mode is silent.
- **A query planner.** `flow`-style multi-step actions with pushdown are
  useful, but not before a single action is airtight.
- **Its own identity provider.** Sluice should consume identity, never own it.
- **Response caching.** Stale data in an agent's context is worse than a slow
  query.

## Known limitations in v0.1

- One role per process (see item 1).
- Rate limits are per process and reset on restart.
- The idempotency store is a local file, so two processes do not share replay
  protection. Move it into the database with the HTTP work.
- Audit rotation is manual and must be done with the process stopped.
- No metrics endpoint; observability is structured logs plus the audit log.
- The in-memory backend has no transactions and is for demos and tests only.
