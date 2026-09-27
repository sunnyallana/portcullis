# Running Sluice

## Pointing at PostgreSQL

```toml
[backend]
kind = "postgres"
dsn = "env:DATABASE_URL"
schemas = ["public"]
max_connections = 10
statement_timeout_secs = 60
```

TLS follows the connection string (`?sslmode=require`, `verify-full`, and so
on); the build uses rustls, so no OpenSSL is needed on the host.

Give Sluice its own database role rather than reusing an application login:

```sql
CREATE ROLE sluice LOGIN PASSWORD '…';
GRANT USAGE ON SCHEMA public TO sluice;
GRANT SELECT ON public.orders TO sluice;
GRANT INSERT ON public.refunds TO sluice;
```

Sluice needs to read `information_schema` for its startup validation, which
every role can do by default.

## First run

```sh
sluice doctor --config /etc/sluice/orders.toml
```

`doctor` checks the file, connects, counts visible tables, validates every
action, reports warnings, and verifies the audit chain. It exits non-zero when
something needs attention, so it belongs in a deploy pipeline. It also flags a
`mask_salt` left at the example value and an audit log with fsync disabled.

Then:

```sh
sluice validate    # actions against the live schema
sluice tools       # exactly what an MCP client will see
```

## Serving

```sh
sluice serve --config /etc/sluice/orders.toml --role support_eu
```

stdout carries the protocol and nothing else. Logs go to stderr; set the level
with `SLUICE_LOG` (`SLUICE_LOG=info`, or `SLUICE_LOG=sluice_engine=debug`).

One process per role. The role fixes the caller's attributes for the lifetime
of the process, which is what scopes every call — see
[security-model.md](security-model.md#identity).

Under systemd:

```ini
[Service]
ExecStart=/usr/local/bin/sluice serve --config /etc/sluice/orders.toml --role support_eu
Environment=DATABASE_URL=postgres://sluice@db/app?sslmode=verify-full
Environment=SLUICE_LOG=info
WorkingDirectory=/var/lib/sluice
User=sluice
```

MCP clients launch the binary themselves, so for Claude Code:

```sh
claude mcp add orders -- sluice serve --config /etc/sluice/orders.toml --role support_eu
```

## Environment variables

| Variable | Effect |
|---|---|
| `SLUICE_CONFIG` | Default `--config` path |
| `SLUICE_ROLE` | Default `--role` |
| `SLUICE_CALLER` | Default caller identity in the audit log |
| `SLUICE_LOG` | Log filter (`error`, `warn`, `info`, `debug`, or per-module) |

Secrets belong in `env:NAME` or `file:/path` references inside the config, not
in the file itself.

## The approval queue

```sh
sluice approvals list
sluice approvals approve apr_c31736cb8b --caller manager-jane
sluice approvals deny    apr_c31736cb8b --caller manager-jane
```

Approving executes the call immediately, re-validating its arguments and
re-checking the caller's role first. A request can be claimed once; a second
attempt fails. Requests expire after `[approvals] ttl_secs`.

The queue is a JSONL event log, so `tail -f sluice-approvals.jsonl` is a
perfectly good pager for a small team, and piping it into Slack is a few lines
of shell. A console is on the roadmap.

## The audit log

```sh
sluice audit tail -n 50
sluice audit verify
```

`verify` exits 2 when the chain is broken and names the line. Ship the file off
the host and keep the printed head digest somewhere separate — that is what
makes tampering provable rather than merely detectable.

Rotation: the file is opened in append mode and the chain continues across
restarts. To rotate, stop the process, move the file, and start again; the new
file begins a new chain, so archive the old head digest with it. Do not rotate
underneath a running process.

`fsync` policy:

| Setting | Behaviour |
|---|---|
| `always` | fsync per record. Use where the log is evidence |
| `batch` | Flush per record, fsync at shutdown. Survives a process crash |
| `never` | Tests only |

## Limits and tuning

- `[limits] max_rows` caps every action. An action asking for more is capped
  and warned about at startup.
- `timeout` per action is a client-side deadline; `statement_timeout_secs` is
  the server-side backstop. Set the backstop above the largest action timeout.
- `rate_limit` is per caller and action, in a one-minute sliding window, held
  in memory. It resets on restart and is not shared between processes.
- Reads fetch one row beyond the limit to detect truncation, and tell the model
  when a result was cut short.

## Upgrading

Action definitions are validated against the live schema at every startup, so a
schema migration that removes a column Sluice uses will fail the next start
rather than fail a tool call. Run `sluice validate` against the new schema
before the migration goes out.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `table X does not exist` at startup | Wrong schema, or the role cannot see it. Check `[backend] schemas` and grants |
| `X exists in more than one schema` | Qualify it: `public.orders` |
| `row_filter needs $caller.region, but role Y does not define it` | Add the attribute to that role, or remove it from its `allow` list |
| Every call returns no rows | The role's attributes do not match any data. Check with `sluice call --role …` |
| `no database connection was free within the pool timeout` | Raise `max_connections`, or find the slow action with `sluice audit tail` |
| A column is missing from `returns` validation | Its PostgreSQL type is not modelled (arrays, ranges, custom types) |
