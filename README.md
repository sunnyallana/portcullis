# Sluice

A governed data-action layer for AI agents.

Sluice sits between an agent and a database and publishes a fixed set of typed,
permissioned **actions** over MCP. The agent chooses an action and fills in its
declared parameters. It never writes SQL, never names a table, never sees a
credential, and never reaches a row outside its scope. Every call — allowed,
refused or parked for approval — lands in a hash-chained audit log.

```
  Agent (Claude, or your own)
        │  MCP over stdio
        ▼
  ┌──────────────────────────────┐
  │  sluice                      │
  │   · declared actions         │  ← one TOML file, validated at startup
  │   · row filters + masking    │
  │   · approval gates           │
  │   · replay protection        │
  │   · append-only audit chain  │
  │   · credentials              │
  └──────────┬───────────────────┘
             │ parameterised SQL, bounded
             ▼
        PostgreSQL
```

## Why

The two things you can do today are both bad. An `execute_sql` MCP server gives
a model the whole database and no security team will sign it off. Hand-written
per-system REST glue is safe and takes a quarter per system.

Sluice is the third option: declare what the agent may do, in a file, checked
against the live schema before anything is published.

## Try it without a database

```sh
cargo build --release
./target/release/sluice init ./demo
cd demo

sluice validate
sluice call --action find_order --arg order_no=8812 --role support_eu --caller alice
sluice call --action find_order --arg order_no=8812 --role support_us --caller bob   # no rows: wrong region
sluice call --action refund_order --arg order_no=8812 --arg amount=1200.00 \
            --arg reason="lost parcel" --role support_eu --caller alice              # parked for approval
sluice approvals list
sluice approvals approve <id> --caller manager-jane
sluice audit verify
```

The demo runs on an in-memory fixture, so there is nothing to install. Point
`[backend]` at PostgreSQL and the same actions work unchanged.

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

## Wiring it to Claude Code

```sh
claude mcp add orders -- sluice serve --config /etc/sluice/orders.toml --role support_eu
```

Or in an MCP client's config:

```json
{
  "mcpServers": {
    "orders": {
      "command": "sluice",
      "args": ["serve", "--config", "/etc/sluice/orders.toml", "--role", "support_eu"],
      "env": { "DATABASE_URL": "postgres://…" }
    }
  }
}
```

## Commands

| Command | What it does |
|---|---|
| `sluice init [dir]` | Write a starter config and demo data |
| `sluice validate` | Check every action against the live schema |
| `sluice doctor` | Config, connectivity, schema, actions and audit in one pass |
| `sluice tools` | Show what an MCP client would see |
| `sluice serve` | Serve MCP on stdin/stdout |
| `sluice call` | Invoke one action from the shell, as a role |
| `sluice approvals list \| approve \| deny` | Work the approval queue |
| `sluice audit verify \| tail` | Check the chain, read recent decisions |

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
- **Nothing happens off the record.** Refusals and parked calls are audited too,
  and `sluice audit verify` detects an edited or deleted line.

## Scope, honestly

v0.1 covers PostgreSQL and MCP over stdio, and does that properly. Not yet
built: HTTP transport with OIDC (today the role is fixed per process by
`--role`), MySQL/SQL Server/Snowflake, the schema profiler that drafts a config
for you, action bundle versioning, and a web console for the approval queue.
See [docs/roadmap.md](docs/roadmap.md).

The in-memory backend is for the demo and the test suite. It is not a database
and refuses to pretend otherwise.

## Building

Rust 1.85 or newer (edition 2024).

```sh
cargo build --release          # ./target/release/sluice
cargo test --workspace         # needs nothing installed
cargo clippy --workspace --all-targets
```

The PostgreSQL backend has its own integration tests, which run against a real
server and are skipped unless one is configured:

```sh
docker run -d --name sluice-pg -e POSTGRES_PASSWORD=sluice-test \
           -e POSTGRES_DB=sluice -p 55432:5432 postgres:18
psql "postgres://postgres:sluice-test@localhost:55432/sluice" \
     -f examples/postgres-schema.sql
SLUICE_TEST_DATABASE_URL="postgres://postgres:sluice-test@localhost:55432/sluice" \
     cargo test -p sluice-db --test postgres_live
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
| `sluice-core` | Values, schema, action specs, masking, audit chain |
| `sluice-sql` | Predicate language and parameterised statement builder |
| `sluice-db` | Backend trait, pooled PostgreSQL, in-memory fixture |
| `sluice-engine` | Config, validation, policy, the request path |
| `sluice-mcp` | MCP server over stdio |
| `sluice-cli` | The `sluice` binary |

## Licence

Not yet decided; the crates are marked `UNLICENSED` so nothing is published by
accident. Settle this before sharing the repository.
