# Security model

What Sluice promises, how it is enforced, and — just as important — what it
does not cover. Read the last section before deploying anything.

## Threat model

The adversary assumed throughout is **the model itself, behaving badly**:
hallucinating, being prompt-injected through data it reads, looping, or being
driven by a user who wants to see someone else's rows. It is not assumed to be
malicious in the sense of running code on the host — if an attacker controls
the machine Sluice runs on, they control Sluice.

Concretely, Sluice is built against these:

| Threat | Control |
|---|---|
| Model composes destructive SQL | It cannot write SQL at all; only declared actions exist |
| Model injects SQL through an argument | Arguments only ever become bind parameters |
| Model reads another tenant's or region's rows | `row_filter` is AND-ed into every read, sourced from identity |
| Model creates a row outside its scope | Row-filter equalities are written into inserts |
| Model exfiltrates PII wholesale | `returns` is a fixed column list; `mask` obscures; `max_rows` bounds |
| Model retries a write after a timeout | `idempotency` returns the first result |
| Model loops and hammers the database | `rate_limit` per caller and action, plus statement timeouts |
| Large or irreversible change slips through | `approval` parks the call for a human |
| Someone disputes what happened | Hash-chained append-only audit log |
| Credentials leak into the context window | Credentials live in the process; the model never sees them |

## How the SQL guarantee is actually enforced

There are three separate barriers, and all three would have to fail.

1. **The grammar.** The predicate language cannot express a function call, a
   subquery, a second table, a semicolon or a comment. The lexer rejects those
   characters outright. See `sluice-sql/src/expr.rs`.
2. **Where names come from.** Table and column names in a statement come from
   the live schema read at startup, never from a request. `Dialect::quote`
   re-checks each identifier against `[A-Za-z0-9_]` immediately before it goes
   into SQL text, so even a bug upstream cannot smuggle punctuation through.
3. **Where values go.** Every value — parameters, caller attributes, literals —
   is bound. `Binder::bind` is the only path to a placeholder. The one
   exception is SQL `NULL`, written as the keyword because a bind parameter has
   no correct type for the absence of a value; `NULL` encodes absence, never a
   caller's input.

The test `an_argument_full_of_sql_is_just_data` fires the usual payloads
through the whole engine and asserts they match nothing and break nothing.

## Scope enforcement

A `row_filter` may reference `$caller.*` and literals only. If it references a
`:parameter`, startup fails: an agent that can supply part of its own filter can
widen it.

Every role permitted to call an action must define every attribute that
action's filter needs. This is checked at startup, so the runtime never has to
decide what to do about a missing attribute — but if one is missing anyway
(a hand-built caller, a future identity source), the call fails closed with
`caller_attribute_missing`. A scope that silently evaluates to "everything" is
the failure this system exists to prevent.

On reads the filter is a `WHERE` clause. On inserts there is no row to test, so
its equalities become column values. A write action whose filter cannot be
expressed that way is refused at validation rather than published with a
weaker guarantee than it looks like it has.

Rows outside scope are **invisible, not forbidden**: the agent gets zero rows,
not "access denied". Denial leaks the existence of the row.

## Identity

v0.1 has one identity per process. `sluice serve --role support_eu` fixes the
role, and every call from that MCP client runs as it. This is honest for stdio,
where the client is a local process the operator launched, and it is why one
server per role is the deployment pattern today.

It is **not** sufficient for a shared multi-user server. HTTP transport with
OIDC, where the caller's token supplies the role and its attributes, is the
next piece of work. Until then, do not put one Sluice process behind a shared
endpoint and assume per-user scoping.

## The audit log

Line-delimited JSON, append-only, each record carrying the digest of the one
before it. `sluice audit verify` replays the chain and reports the first line
where it breaks, so an edited or deleted record is detectable. The head digest
is printed; recording it elsewhere — a log shipper, a WORM bucket, a weekly
email — is what turns detection into proof, because someone who can rewrite the
whole file can also recompute the whole chain.

Recorded for every call: timestamp, request id, caller, role, action, arguments
(masked), decision, rows, duration, error code, approval id. Refusals and parked
calls are recorded too.

The writer is a dedicated thread with a bounded queue. When the queue is full,
callers wait. Records are never dropped, because "we were too busy to log it" is
not an acceptable answer.

## What this does not do

- **It is not a database firewall.** Anything else with the same credentials can
  still do anything. Give Sluice its own database role with only the grants its
  actions need — that is the belt to this brace.
- **It does not stop a user who legitimately has access from misusing it.** It
  bounds and records what happens; it does not judge intent.
- **It does not sanitise what the model reads.** If a row contains an injection
  payload and your agent acts on it, Sluice's protection is that the action set
  is small and writes are gated. That is a real reduction, not immunity.
- **It does not encrypt anything at rest.** Audit and approval files sit on disk
  in plain text. Put them on an encrypted volume; they contain masked arguments,
  not secrets, but they do contain business facts.
- **It has had no external audit.** The controls are tested, not certified.

## Operational recommendations

- Give the connection a database role restricted to the tables the actions use.
- Set `mask_salt` to something from a secret store, not the example value.
- Keep `fsync = "always"` where the audit trail is evidence.
- Ship the audit log off the host; keep the head digest somewhere separate.
- Run `sluice doctor` in your deploy pipeline. It fails the pipeline on a
  broken chain, an unreachable backend or a leftover example salt.
- One process per role until HTTP and OIDC land.
