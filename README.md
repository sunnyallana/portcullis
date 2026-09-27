<div align="center">

# Portcullis

**A governed data-action layer for AI agents.**

Declare what an agent may do with your database, in one file.
It never writes SQL, never names a table, never sees a credential,
and never reaches a row outside its scope.

[![CI](https://github.com/sunnyallana/portcullis/actions/workflows/ci.yml/badge.svg)](https://github.com/sunnyallana/portcullis/actions/workflows/ci.yml)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-b7410e.svg)](https://www.rust-lang.org)
[![Edition 2024](https://img.shields.io/badge/edition-2024-555.svg)](https://doc.rust-lang.org/edition-guide/)
[![Image ~6 MB](https://img.shields.io/badge/image-~6%20MB%20distroless-2b7489.svg)](docs/operations.md#the-container-image)
[![unsafe forbidden](https://img.shields.io/badge/unsafe-forbidden-success.svg)](Cargo.toml)

[Quick start](#quick-start) · [How it works](#how-it-works) ·
[Action format](docs/action-format.md) · [Operations](docs/operations.md) ·
[Security model](docs/security-model.md) · [Roadmap](docs/roadmap.md)

</div>

---

## The problem

The two things you can do today are both bad.

An `execute_sql` MCP server hands a model the whole database. No security team
signs that off. Hand-written per-system REST glue is safe and takes a quarter
per system, then rots.

Portcullis is the third option. You declare a fixed set of typed, permissioned
**actions** in a TOML file. Every one is checked against the live schema before
a single tool is published. The agent picks an action and fills in its declared
parameters, and that is the entire surface it can reach.

Every call — allowed, refused, or parked for a human — lands in a hash-chained
audit log you can verify.

## How it works

```mermaid
%%{init: {"flowchart": {"htmlLabels": true, "wrappingWidth": 400, "rankSpacing": 34, "nodeSpacing": 34, "curve": "basis"}}}%%
flowchart TB
    agent(["<b>Agent</b> · Claude, your own, or the CLI"])
    agent -->|"tools/call · stdio, or HTTP with a bearer token"| g1

    g1["<b>1 · Identity</b> — OIDC claims, an API key, or a fixed role"]
    g2["<b>2 · Grant</b> — may this role run this action at all?"]
    g3["<b>3 · Parameters</b> — typed, bounded, <code>one_of</code>, <code>max_len</code>"]
    g4["<b>4 · Budget</b> — rate limit, replay window, timeout"]
    g5["<b>5 · Approval</b> — over the threshold? park it, do not run it"]
    g6["<b>6 · Statement</b> — row filter AND-ed in, arguments as binds only"]
    g7["<b>7 · Masking</b> — partial, last4, hash"]
    out(["<b>Rows the caller is allowed to see</b>, or a refusal"])

    g1 --> g2 --> g3 --> g4 --> g5 --> g6
    g6 -->|"parameterised SQL, bounded"| db[("PostgreSQL · MySQL · SQL Server")]
    db --> g7 --> out

    g5 -.->|"parked write"| approvals["<b>Approvals</b> — console and JSON API<br/><i>approver_roles · no self-approval</i>"]
    approvals -.->|"a human releases it"| g6
    g5 -.-> audit
    g7 -.-> audit
    audit["<b>Audit log</b> — hash-chained, append-only<br/><i>allowed, refused and parked alike · sealed segments · verifiable</i>"]

    classDef step fill:#eef4ff,stroke:#3b6ea5,color:#0b1220;
    classDef side fill:#fff6e6,stroke:#b8860b,color:#0b1220;
    classDef store fill:#e8f6ef,stroke:#2e8b57,color:#0b1220;
    classDef actor fill:#f3eaff,stroke:#7a4fb5,color:#0b1220;
    class g1,g2,g3,g4,g5,g6,g7 step;
    class approvals,audit side;
    class db store;
    class agent,out actor;
```

Everything in the middle column is configuration, not code. The stages run in
that order on every call, and none of them can be skipped by anything the agent
sends.

### What a single call actually does

```mermaid
sequenceDiagram
    autonumber
    participant A as Agent
    participant P as Portcullis
    participant D as Database
    participant L as Audit log

    A->>P: refund_order(order_no, amount, reason)
    P->>P: token → caller id, role, attributes (region = EU)
    P->>P: role grants refund_order? types and bounds ok?
    P->>P: seen this idempotency key before?
    alt amount over the approval threshold
        P->>L: parked (who, what, why)
        P-->>A: awaiting approval · request id
        Note over P: a human approves in the console, and the write runs then, for the approver
    else within the threshold
        P->>D: INSERT … VALUES (bind, bind, …), region = 'EU' written in
        D-->>P: 1 row
        P->>D: SELECT the row back by key — RETURNING on PostgreSQL, which MySQL and SQL Server lack
        D-->>P: refund_id, amount, issued_at
        P->>L: allowed (caller, action, args digest, rows)
        P-->>A: masked columns only
    end
```

The agent never learns the table name, the row filter, or that a second
statement ran.

## Quick start

No database needed — the demo runs on an in-memory fixture.

```sh
cargo build --release
./target/release/portcullis init ./demo
cd demo

portcullis validate
portcullis call --action find_order --arg order_no=8812 --role support_eu --caller alice
portcullis call --action find_order --arg order_no=8812 --role support_us --caller bob
#   ↑ no rows: wrong region, and nothing told the agent why

portcullis call --action refund_order --arg order_no=8812 --arg amount=1200.00 \
            --arg reason="lost parcel" --role support_eu --caller alice
#   ↑ over the threshold, so it is parked rather than written

portcullis approvals list
portcullis approvals approve <id> --role support_eu --caller manager-jane
portcullis audit verify
```

Point `[backend]` at a real database and the same actions work unchanged.

### Install

```sh
docker pull ghcr.io/sunnyallana/portcullis:0.2.0
```

Or take a binary from the [latest release](https://github.com/sunnyallana/portcullis/releases/latest):
static musl for Linux x86_64, arm64 for macOS, x86_64 for Windows. The Linux
build has no runtime dependencies.

The image is about 6 MB on `distroless/static` — no shell, no package manager,
no libc — and runs as a non-root user. It refuses to serve on anything but
loopback without an `[auth]` block, so configure authentication before you
publish a port. [Deployment details](docs/operations.md#the-container-image).

### Point it at a database you already have

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

Full reference: **[docs/action-format.md](docs/action-format.md)**.

## Two ways to serve it

<table>
<tr><th width="50%">stdio</th><th width="50%">HTTP</th></tr>
<tr valign="top"><td>

For a client that launches the process itself. One role per process.

```sh
claude mcp add orders -- \
  portcullis serve \
    --config /etc/portcullis/orders.toml \
    --role support_eu
```

</td><td>

When many people share one deployment and each needs their own scope.

```sh
portcullis serve --http
```

The caller's token decides their role, per request.

</td></tr>
</table>

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

Only claims the operator maps become attributes, so an identity provider that
starts emitting a new claim cannot silently widen anyone's access. API keys
(`portcullis apikey --role batch`) cover machine callers. `kind = "none"` exists
for local development and refuses to bind anything but loopback.

The same process serves a small approvals console at `/`, a JSON approvals API,
`/healthz`, `/readyz` and Prometheus metrics at `/metrics`.

## What is guaranteed

| | |
|---|---|
| **No SQL from the model** | Agent input only ever becomes a bind parameter. The predicate language has no functions, no subqueries, and cannot name a second table. ([security model](docs/security-model.md)) |
| **Scope cannot be argued away** | A `row_filter` references only `$caller.*` and literals, is AND-ed into every read, and is written into the row on every insert. An action whose row filter could not be enforced on a write fails validation rather than shipping. |
| **Typos fail at startup, not at 3am** | Tables, columns, parameter types, masks, keys and role grants are all checked against the live schema before a single action is published. |
| **Retries are not duplicate writes** | A write action nominates the parameters that identify it; an identical retry returns the first result. |
| **A gate the requester can open is not a gate** | Self-approval is refused by default, and `approver_roles` restricts who may release anything. |
| **Nothing happens off the record** | Refusals and parked calls are audited too, and `portcullis audit verify` detects an edited or deleted line — across sealed rotation segments, not just the live file. |
| **You can prove a change did nothing** | `portcullis replay --record` captures what reads return today; `--against` re-runs them later and exits 3 if anything differs. |
| **A change can be rolled back in a second** | Several versions of the action set are served at once. Promotion and rollback move an alias; neither edits the file that is currently serving traffic. |

## Commands

| Command | What it does |
|---|---|
| `portcullis init [dir]` | Write a starter config and demo data |
| `portcullis profile` | Read a database and draft a configuration for it |
| `portcullis validate` | Check every action against the live schema |
| `portcullis doctor` | Config, connectivity, schema, actions and audit in one pass |
| `portcullis tools` | Show what an MCP client would see |
| `portcullis bundles` | Show the versions loaded and who gets which |
| `portcullis serve [--http]` | Serve MCP on stdio, or over HTTP |
| `portcullis call` | Invoke one action from the shell, as a role |
| `portcullis approvals list \| approve \| deny` | Work the approval queue |
| `portcullis replay [--record \| --against]` | Re-run recorded reads and diff them |
| `portcullis audit verify \| tail \| segments \| rotate` | Check the chain, read decisions, seal history |
| `portcullis apikey --role R` | Mint a key and print the config to paste |

Add `--json` to any of them for machine-readable output. Logs go to stderr;
`serve` owns stdout.

## Backends

| | Status | Notes |
|---|---|---|
| **PostgreSQL** | default feature | Pooled, TLS via rustls. Live suite against PostgreSQL 18. |
| **MySQL** | `--features mysql` (on by default in the CLI) | No UUID type: use `CHAR(36)`. `returning` costs a second round trip. Live suite against MySQL 9. |
| **SQL Server** | `--features mssql` | Its own driver stack — `sqlx` dropped MSSQL after 0.6 — so `@P1` binds, `[bracket]` quoting, `TOP (n)` instead of `LIMIT`. No `RETURNING` and no upsert. Live suite against SQL Server 2022. |
| **In-memory** | always | The demo and the test suite. Not a database, and refuses to pretend otherwise. |
| Snowflake, BigQuery | not built | Deliberately. [Why](docs/roadmap.md#next-in-order). |

## Scope, honestly

Rate limits and the idempotency store are per process unless you set
`[limits] store = "database"`, and the shared window is a fixed minute rather
than a sliding one. Archiving sealed audit segments off the host is manual. An
agent has to poll `approval_status`; nothing calls it back. `/metrics` is
unauthenticated by design, so keep it off public interfaces. There has been no
external security audit: the controls are tested, not certified.

The full list is in [docs/roadmap.md](docs/roadmap.md), including the things
that were considered and deliberately not built.

## Building

Rust 1.85 or newer (edition 2024).

```sh
cargo build --release                        # ./target/release/portcullis
cargo build --release --features mssql       # with SQL Server
cargo test --workspace                       # needs nothing installed
cargo clippy --workspace --all-targets --all-features
```

<details>
<summary><b>Running the live backend tests</b> (each skips unless its server is configured)</summary>

```sh
# PostgreSQL
docker run -d --name portcullis-pg -e POSTGRES_PASSWORD=portcullis-test \
           -e POSTGRES_DB=portcullis -p 55432:5432 postgres:18
psql "postgres://postgres:portcullis-test@localhost:55432/portcullis" -f examples/postgres-schema.sql
PORTCULLIS_TEST_DATABASE_URL="postgres://postgres:portcullis-test@localhost:55432/portcullis" \
     cargo test -p portcullis-db --test postgres_live

# MySQL
docker run -d --name portcullis-mysql -e MYSQL_ROOT_PASSWORD=portcullis-test \
           -e MYSQL_DATABASE=portcullis -p 33306:3306 mysql:9
mysql -h127.0.0.1 -P33306 -uroot -pportcullis-test portcullis < examples/mysql-schema.sql
PORTCULLIS_TEST_MYSQL_URL="mysql://root:portcullis-test@localhost:33306/portcullis" \
     cargo test -p portcullis-db --features mysql --test mysql_live

# SQL Server
docker run -d --name portcullis-mssql -e ACCEPT_EULA=Y \
  -e MSSQL_SA_PASSWORD=Portcullis-test1 -e MSSQL_PID=Developer \
  -p 21433:1433 mcr.microsoft.com/mssql/server:2022-latest
SQLCMD="docker run --rm --network host -v $PWD/examples:/examples:ro \
  mcr.microsoft.com/mssql-tools:latest /opt/mssql-tools/bin/sqlcmd \
  -S localhost,21433 -U sa -P Portcullis-test1 -b"
$SQLCMD -Q "CREATE DATABASE portcullis"
$SQLCMD -d portcullis -i /examples/sqlserver-schema.sql
PORTCULLIS_TEST_MSSQL_DSN="Server=tcp:localhost,21433;User Id=sa;Password=Portcullis-test1;Database=portcullis;TrustServerCertificate=true" \
     cargo test -p portcullis-db --features mssql --test mssql_live
```

</details>

<details>
<summary><b>Building on Windows</b> (two Visual Studio components, and a linker trap)</summary>

The default MSVC toolchain needs two components for its linker. Installing the
IDE alone is not enough:

```powershell
& "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\setup.exe" modify `
  --installPath "C:\Program Files\Microsoft Visual Studio\18\Community" `
  --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
  --add Microsoft.VisualStudio.Component.Windows11SDK.26100 `
  --quiet --norestart
```

Build from PowerShell rather than Git Bash. Git for Windows ships its own
`/usr/bin/link.exe`, which shadows the MSVC linker and produces confusing
`extra operand` errors.

</details>

## Layout

| Crate | Contents |
|---|---|
| `portcullis-core` | Values, schema, action specs, masking, audit chain |
| `portcullis-sql` | Predicate language, parameterised statement builder, dialects |
| `portcullis-db` | Backend trait; pooled PostgreSQL, MySQL and SQL Server; in-memory fixture |
| `portcullis-engine` | Config, validation, policy, request path, bundles, profiler, replay |
| `portcullis-mcp` | MCP protocol and the stdio transport |
| `portcullis-http` | HTTP transport, OIDC and API keys, approvals API, console |
| `portcullis-cli` | The `portcullis` binary |

## Contributing

`CONTRIBUTING.md` covers the conventions that are not obvious from the code:
never typing the product name in a `.rs` file, how to add an environment
variable or a metric series so the consistency tests see it, and the rename
procedure including the one decision in it that is not mechanical.

## Licence

Not yet decided. The crates are marked `UNLICENSED` and `publish = false` so
nothing is published by accident. Settle this before sharing the repository.
