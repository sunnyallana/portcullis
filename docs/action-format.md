# The configuration file

One TOML file describes a deployment: where the data is, who may do what, and
which actions exist. Unknown keys are an error, not a shrug — a misspelled
`max_row` that silently never applied would be worse than a failed startup.

Paths inside the file are resolved relative to the file itself.

---

## `[server]`

| Key | Required | Meaning |
|---|---|---|
| `name` | yes | Server name reported to MCP clients |
| `mask_salt` | no | Salt for `hash` masking. Defaults to `name`. Changing it changes every digest. Accepts `env:` / `file:` |

## `[backend]`

```toml
[backend]
kind = "postgres"
dsn = "env:DATABASE_URL"
schemas = ["public"]
max_connections = 10
min_connections = 1
statement_timeout_secs = 60
```

| Key | Applies to | Meaning |
|---|---|---|
| `kind` | both | `"postgres"`, `"mysql"` or `"memory"` |
| `dsn` | postgres, mysql | Connection string. Use `env:NAME` or `file:/path` |
| `schemas` | postgres, mysql | Schemas (MySQL: databases) to expose. Empty means every non-system schema, or the one in the connection string |
| `max_connections`, `min_connections` | postgres, mysql | Pool bounds |
| `statement_timeout_secs` | postgres, mysql | Server-side backstop on every connection |
| `fixtures` | memory | Path to a JSON fixture (demo and tests only) |

Columns whose type Portcullis does not model are left out of the schema: arrays,
ranges, `tsvector` and custom types on PostgreSQL; blobs, spatial and bit types
on MySQL. An action that names one fails validation rather than returning a
mis-decoded value.

MySQL has no UUID type and no `RETURNING`. Model a UUID column as `CHAR(36)`
and declare the parameter `text`. When an action asks for `returning`, the
backend re-selects the row on the same connection using the primary key values
the write supplied, or `LAST_INSERT_ID()` for a single auto-increment key; if
neither is available the action fails validation at first use with a message
saying so.

## `[audit]`

| Key | Default | Meaning |
|---|---|---|
| `path` | `portcullis-audit.jsonl` | Where the log is written |
| `fsync` | `batch` | `always` (fsync per record), `batch` (flush per record, fsync at shutdown), `never` (tests only) |

## `[approvals]`

| Key | Default | Meaning |
|---|---|---|
| `path` | `portcullis-approvals.jsonl` | Durable queue of parked calls |
| `ttl_secs` | `604800` | How long a request can wait before it expires |
| `approver_roles` | `[]` | Roles allowed to release a parked call. Empty means any role, which `portcullis doctor` warns about when HTTP is enabled |
| `allow_self_approval` | `false` | Whether the caller who raised a request may decide it |

## `[limits]`

| Key | Default | Meaning |
|---|---|---|
| `max_rows` | `1000` | Hard ceiling; caps any action's own `max_rows` |
| `max_request_bytes` | `65536` | Largest accepted argument object |
| `idempotency_ttl_secs` | `86400` | How long a completed write is remembered |

## `[http]`

Present only when the deployment serves HTTP. `portcullis serve --http` uses it.

| Key | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:8080` | Address to bind. `--http <address>` overrides it |
| `console` | `true` | Serve the approvals console at `/` |
| `max_body_bytes` | `262144` | Largest accepted request body |
| `request_timeout_secs` | `60` | Deadline for one request |

TLS is not terminated here. Put a reverse proxy in front, or bind loopback and
proxy on the same host.

## `[auth]`

How HTTP callers prove who they are. Ignored by the stdio transport, which
takes its role from `--role`.

```toml
[auth]
kind = "oidc"
issuer = "https://id.example.com/"
audience = ["portcullis"]
jwks_url = "https://id.example.com/.well-known/jwks.json"   # discovered from the issuer when absent
role_claim = "portcullis_role"
caller_claim = "sub"
attribute_claims = { region = "region", tenant = "tid" }
role_map = { "support-eu" = "support_eu" }
leeway_secs = 60
refresh_secs = 300
```

The role claim may be a string, a space-separated list or an array; each value
passes through `role_map` and the first one this deployment knows is used.

`attribute_claims` is the whole of it: a claim that is not listed there never
becomes a caller attribute. Attributes from a token override the role's
configured defaults, which is the point — the role says what a support agent
may do, the token says which region this one covers.

`jwks_url` also accepts `file:/path/to/jwks.json` for an air-gapped install
that ships the key material alongside the configuration. Keys are fetched at
startup, so a wrong URL fails the boot rather than the first request, and
re-fetched when they go stale or a token arrives with an unknown `kid`.

```toml
[auth]
kind = "api_key"

[[auth.key]]
hash = "…64 hex characters…"    # mint with `portcullis apikey --role batch`
role = "batch"
caller = "nightly-reconcile"
attributes = { region = "EU" }
```

Only the digest is stored, so the file never holds anything replayable.
Comparison is constant time.

```toml
[auth]
kind = "none"
role = "support_eu"
caller = "local"
```

Development only. The server refuses to bind anything but loopback in this
state.

## `[[role]]`

```toml
[[role]]
name = "support_eu"
allow = ["find_order", "refund_order"]     # or ["*"]
attributes = { region = "EU", tenant = 42 }
```

`attributes` are the only values a `row_filter` can reference through
`$caller.*`. They come from here, never from a tool argument. Two keys are
always available and cannot be overridden: `$caller.id` and `$caller.role`.

A role that allows an action which does not exist is an error, with a
suggestion. So is a role that allows nothing.

---

## `[action.<name>]`

| Key | Required | Meaning |
|---|---|---|
| `description` | yes | One sentence. The model reads this to decide when to call it |
| `table` | yes | Table or view. Unqualified names resolve when unambiguous |
| `kind` | no | `read` or `write`; inferred from the presence of `write` |
| `params` | no | Declared parameters (below) |
| `returns` | reads | Columns the action exposes. An empty list is refused |
| `filter` | no | Predicate over the table, may use `:params` |
| `row_filter` | no | Predicate injected into every call; `$caller.*` and literals only |
| `order_by` | no | e.g. `["placed_at desc", "order_no"]` |
| `mask` | no | Per-column masking applied to results and to logged arguments |
| `max_rows` | no (100) | Row ceiling, capped by `[limits] max_rows` |
| `timeout` | no (`30s`) | Statement timeout: `250ms`, `30s`, `2m`, or bare seconds |
| `rate_limit` | no | Calls per minute, per caller, for this action |

### Parameters

Short form, or the full table:

```toml
params = { order_no = "text" }

params = { status = { type = "text", required = false,
                      one_of = ["open", "held"],
                      description = "Restrict to one status",
                      max_len = 16 } }
```

Types: `bool`, `int`, `float`, `decimal`, `text`, `timestamp`, `uuid`, `json`.
Friendly aliases are accepted (`string`, `bigint`, `numeric`, `timestamptz`, …).
Parameters are required unless `required = false`.

Use `decimal` for money. `float` is refused as an approval threshold, because
comparing `500.0000000001` to a limit is not a rounding error anyone wants.

Arguments are type-checked strictly at the protocol boundary: a text parameter
rejects the number `8812`. The CLI reads `--arg` values against the declared
type, so `--arg order_no=8812` still does the obvious thing.

### An omitted optional parameter drops its predicate

```toml
filter = "status in ('open','held') and status = :status and total >= :min_total"
```

Call it with no arguments and both optional tests disappear, leaving the
`in (...)` test. Supply `min_total` and that test comes back. This only works
inside a top-level `and`: an optional parameter under `or` or `not` is refused
at compile time, because dropping it there would *widen* the result.

### Masking

| Mask | Effect |
|---|---|
| `partial` | `alice@example.com` → `a***@example.com`; other text keeps its first and last character |
| `last4` | `4111111111111111` → `************1111` |
| `hash` | Stable salted digest; equal values stay equal |
| `redact` | `[redacted]` |
| `none` | No change |

Masking happens after the database answers, so a filter can still match on the
real value while the model only sees the masked form. Null is never disguised
as a value.

---

## `[action.<name>.write]`

```toml
[action.refund_order.write]
mode        = "insert"                 # insert | upsert | update
columns     = { refund_id = "uuid()", order_no = ":order_no",
                amount = ":amount", issued_by = "$caller.id", issued_at = "now()" }
keys        = []                       # required for upsert and update
idempotency = ["order_no", "amount"]
returning   = ["refund_id", "amount"]
```

Each value in `columns` is one term: `:param`, `$caller.field`, `now()`,
`uuid()`, or a literal (`'manual'`, `0`, `true`, `null`). Types are checked
against the column at startup: `now()` on a non-timestamp column is an error.

`idempotency` names the parameters that identify a call. The key is a digest of
the action, the caller and those values, so one caller retrying is one write
while two callers issuing the same refund are two. Omitting it earns a warning:
agents retry.

On an insert or upsert, a `row_filter` is not a `WHERE` clause — there is no row
to test yet — so its values are written into the row instead. A caller scoped to
`EU` cannot create a `US` row even if the action's own `columns` say otherwise.
That requires the filter to be a conjunction of `column = value` tests; anything
more complex is refused at validation for write actions. Updates keep the filter
in the `WHERE` clause and leave the row's own values alone.

## `[action.<name>.approval]`

```toml
[action.refund_order.approval]
always      = false
over_param  = "amount"
over_amount = "500.00"
```

Either `always = true` or an `over_param` / `over_amount` pair. When the gate
trips, nothing is executed: the call is parked, the caller gets the request id,
and the log records a `pending` decision. Releasing it re-validates the
arguments and re-checks the role, so a permission removed in the meantime is
not handed back. A request can be claimed once.

---

## The predicate language

```
expr      := or
or        := and ("or" and)*
and       := unary ("and" unary)*
unary     := "not" unary | "(" expr ")" | predicate
predicate := column op term
           | column "is" ["not"] "null"
           | column ["not"] "in" "(" term {"," term} ")"
op        := "=" | "!=" | "<>" | "<" | "<=" | ">" | ">=" | "like"
term      := ":" name | "$caller." name | number | 'text' | true | false | null
```

The left side of a comparison is always a bare column name. There are no
functions, no arithmetic, no subqueries and no way to mention a second table.
Text literals use single quotes, doubled to escape (`'O''Brien'`). Comment
syntax and `;` are not part of the language and are rejected by the lexer.

Comparing two columns is refused, with a message saying so — it is almost
always a missing `:` or missing quotes.
