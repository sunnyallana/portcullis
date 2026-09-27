# Portcullis

A governed data-action layer for AI agents.

Portcullis sits between an agent and a database and publishes a fixed set of typed,
permissioned **actions** over MCP. The agent chooses an action and fills in its
declared parameters. It never writes SQL, never names a table, never sees a
credential, and never reaches a row outside its scope. Every call — allowed,
refused or parked for approval — lands in a hash-chained audit log.

```
  Agents (Claude, or your own)
        │  MCP over stdio, or HTTP with a bearer token
        ▼
  ┌──────────────────────────────┐
  │  portcullis                      │
  │   · declared actions         │  ← one TOML file, validated at startup
  │   · identity per request     │  ← OIDC claims or API keys
  │   · row filters + masking    │
  │   · approval gates           │
  │   · replay protection        │
  │   · append-only audit chain  │
  │   · credentials              │
  └──────────┬───────────────────┘
             │ parameterised SQL, bounded
             ▼
      PostgreSQL · MySQL
```

## Why

The two things you can do today are both bad. An `execute_sql` MCP server gives
a model the whole database and no security team will sign it off. Hand-written
per-system REST glue is safe and takes a quarter per system.

Portcullis is the third option: declare what the agent may do, in a file, checked
against the live schema before anything is published.

## Try it without a database

```sh
cargo build --release
./target/release/portcullis init ./demo
cd demo

portcullis validate
portcullis call --action find_order --arg order_no=8812 --role support_eu --caller alice
portcullis call --action find_order --arg order_no=8812 --role support_us --caller bob   # no rows: wrong region
portcullis call --action refund_order --arg order_no=8812 --arg amount=1200.00 \
            --arg reason="lost parcel" --role support_eu --caller alice              # parked for approval
portcullis approvals list
portcullis approvals approve <id> --role support_eu --caller manager-jane
portcullis audit verify
```

The demo runs on an in-memory fixture, so there is nothing to install. Point
`[backend]` at PostgreSQL or MySQL and the same actions work unchanged.

## Point it at a database you already have

```sh
portcullis profile --dsn "env:DATABASE_URL" --out draft.toml
```

That reads the catalogue, samples rows, guesses what each column holds —
Luhn-checked card numbers, email shapes, credential-looking names — and writes
a draft configuration with masks filled in and a row filter suggested wherever
a tenant or region column was spotted. Reads only, and no sampled value is
printed in the clear. Edit it down, then `portcullis validate`.

## What an action looks like

```toml
[action.find_order]
description = "Look up one order by its order number."
table       = "orders"
params      = { order_no = { type = "text", required = true, max_len = 32 } }
returns     = ["order_no", "status", "total", "customer_email"]
filter      = "order_no = :order_no"
row_filter  = "region = $caller.region"      # injected; the agent cannot see or change it
mask        = { customer_email = "partial" }
max_rows    = 1

[action.refund_order]
description = "Issue a refund. Above 500 it waits for a human."
table       = "refunds"
params      = { order_no = { type = "text", required = true },
                amount   = { type = "decimal", required = true },
                reason   = { type = "text", required = true, max_len = 280 } }
row_filter  = "region = $caller.region"

[action.refund_order.write]
mode        = "insert"
columns     = { refund_id = "uuid()", order_no = ":order_no", amount = ":amount",
                reason = ":reason", issued_by = "$caller.id", issued_at = "now()" }
idempotency = ["order_no", "amount"]          # a retry does not refund twice
returning   = ["refund_id", "amount"]

[action.refund_order.approval]
over_param  = "amount"
over_amount = "500.00"
```

Full reference: [docs/action-format.md](docs/action-format.md).

## Two ways to serve it

**stdio**, for a client that launches the process itself:

```sh
claude mcp add orders -- portcullis serve --config /etc/portcullis/orders.toml --role support_eu
```

**HTTP**, when many people share one deployment and each needs their own scope:

```toml
[http]
listen = "127.0.0.1:8080"

[auth]
kind = "oidc"
issuer = "https://id.example.com/"
audience = ["portcullis"]
role_claim = "portcullis_role"
attribute_claims = { region = "region" }
```

```sh
portcullis serve --http
```

The caller's token decides their role and their scope, per request. Only claims
the operator maps become attributes, so an identity provider that starts
emitting a new claim cannot silently widen anyone's access. API keys
(`portcullis apikey --role batch`) cover machine callers. `kind = "none"` exists for
local development and refuses to bind anything but loopback.

The same process serves a small approvals console at `/`, a JSON approvals API,
`/healthz`, `/readyz` and Prometheus metrics at `/metrics`.

## Commands

| Command | What it does |
|---|---|
| `portcullis init [dir]` | Write a starter config and demo data |
| `portcullis profile` | Read a database and draft a configuration for it |
| `portcullis validate` | Check every action against the live schema |
| `portcullis doctor` | Config, connectivity, schema, actions and audit in one pass |
| `portcullis tools` | Show what an MCP client would see |
| `portcullis serve [--http]` | Serve MCP on stdio, or over HTTP |
| `portcullis call` | Invoke one action from the shell, as a role |
| `portcullis approvals list \| approve \| deny` | Work the approval queue |
| `portcullis replay [--record \| --against]` | Re-run recorded reads and diff them |
| `portcullis audit verify \| tail` | Check the chain, read recent decisions |
| `portcullis apikey --role R` | Mint a key and print the config to paste |

Add `--json` to any of them for machine-readable output. Logs go to stderr;
`serve` owns stdout.

## What is guaranteed

- **No SQL from the model.** Agent input only ever becomes a bind parameter.
  The predicate language has no functions, no subqueries and cannot name a
  second table. ([docs/security-model.md](docs/security-model.md))
- **Scope cannot be argued away.** A `row_filter` references only `$caller.*`
  and literals, is AND-ed into every read, and is written into the row on every
  insert. An action whose row filter could not be enforced on a write fails
  validation rather than shipping.
- **Typos fail at startup, not at 3am.** Tables, columns, parameter types,
  masks, keys and role grants are all checked against the live schema before a
  single action is published.
- **Retries are not duplicate writes.** A write action nominates the parameters
  that identify it; an identical retry returns the first result.
- **An approval gate the requester can open is not a gate.** Self-approval is
  refused by default, and `approver_roles` restricts who may release anything.
- **Nothing happens off the record.** Refusals and parked calls are audited too,
  and `portcullis audit verify` detects an edited or deleted line.
- **You can prove a change did not change behaviour.** `portcullis replay --record`
  captures what reads return today; `--against` re-runs them later and exits 3
  if anything differs.

## Scope, honestly

Built and tested: PostgreSQL and MySQL, MCP over stdio and HTTP, OIDC and API
key authentication, the profiler, replay, the approvals console and queue.

Not built: versioned action bundles with aliases and canary promotion — today a
configuration change is a file edit and a restart. SQL Server, Snowflake and
BigQuery. Multi-step actions with pushdown. See
[docs/roadmap.md](docs/roadmap.md) for why, in that order.

Rate limits and the idempotency store are per process, so a horizontally scaled
deployment enforces them per replica. The in-memory backend is for the demo and
the test suite; it is not a database and refuses to pretend otherwise.

## Building

Rust 1.85 or newer (edition 2024).

```sh
cargo build --release          # ./target/release/portcullis
cargo test --workspace         # needs nothing installed
cargo clippy --workspace --all-targets
```

The database backends have integration tests that run against real servers and
skip unless one is configured:

```sh
docker run -d --name portcullis-pg -e POSTGRES_PASSWORD=portcullis-test \
           -e POSTGRES_DB=portcullis -p 55432:5432 postgres:18
psql "postgres://postgres:portcullis-test@localhost:55432/portcullis" -f examples/postgres-schema.sql
PORTCULLIS_TEST_DATABASE_URL="postgres://postgres:portcullis-test@localhost:55432/portcullis" \
     cargo test -p portcullis-db --test postgres_live

docker run -d --name portcullis-mysql -e MYSQL_ROOT_PASSWORD=portcullis-test \
           -e MYSQL_DATABASE=portcullis -p 33306:3306 mysql:9
mysql -h127.0.0.1 -P33306 -uroot -pportcullis-test portcullis < examples/mysql-schema.sql
PORTCULLIS_TEST_MYSQL_URL="mysql://root:portcullis-test@localhost:33306/portcullis" \
     cargo test -p portcullis-db --features mysql --test mysql_live
```

On Windows, the default MSVC toolchain needs two Visual Studio components for
its linker. Installing the IDE alone is not enough:

```powershell
& "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\setup.exe" modify `
  --installPath "C:\Program Files\Microsoft Visual Studio\18\Community" `
  --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
  --add Microsoft.VisualStudio.Component.Windows11SDK.26100 `
  --quiet --norestart
```

Build from PowerShell rather than Git Bash: Git for Windows ships its own
`/usr/bin/link.exe`, which shadows the MSVC linker and produces confusing
`extra operand` errors.

## Layout

| Crate | Contents |
|---|---|
| `portcullis-core` | Values, schema, action specs, masking, audit chain |
| `portcullis-sql` | Predicate language, parameterised statement builder, dialects |
| `portcullis-db` | Backend trait, pooled PostgreSQL and MySQL, in-memory fixture |
| `portcullis-engine` | Config, validation, policy, request path, profiler, replay |
| `portcullis-mcp` | MCP protocol and the stdio transport |
| `portcullis-http` | HTTP transport, OIDC and API keys, approvals API, console |
| `portcullis-cli` | The `portcullis` binary |

## Licence

Not yet decided; the crates are marked `UNLICENSED` and `publish = false` so
nothing is published by accident. Settle this before sharing the repository.
