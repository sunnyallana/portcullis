# Running Portcullis

## Installing

Tagged releases publish three archives and a container image.

```sh
# binaries: linux x86_64 (static, musl), macOS arm64, Windows x86_64
curl -LO https://github.com/sunnyallana/portcullis/releases/latest/download/SHA256SUMS
curl -LO https://github.com/sunnyallana/portcullis/releases/latest/download/portcullis-v0.2.0-x86_64-unknown-linux-musl.tar.gz
sha256sum -c SHA256SUMS --ignore-missing
tar xzf portcullis-v0.2.0-x86_64-unknown-linux-musl.tar.gz
```

```sh
docker pull ghcr.io/sunnyallana/portcullis:0.2.0
```

The Linux binary is statically linked against musl, so it runs on any glibc or
musl distribution with no runtime dependencies at all.

### Verifying what you received

Every release is signed and carries build provenance. There is no signing key
to distribute: the identity is the release workflow in this repository, proved
through Sigstore.

```sh
# the container image
cosign verify ghcr.io/sunnyallana/portcullis:0.2.0 \
  --certificate-identity-regexp '^https://github\.com/sunnyallana/portcullis/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# any release artifact: which workflow built it, from which commit
gh attestation verify portcullis-v0.2.0-x86_64-unknown-linux-musl.tar.gz \
  --repo sunnyallana/portcullis
```

A verification that fails is not a formality. It means the artifact did not
come from this repository's release workflow, and you should not run it.

A CycloneDX SBOM ships with every release as
`portcullis-v<version>.cdx.json` — around 300 components, each with its
version and licence — and inside the offline bundle as `portcullis.cdx.json`.

### Air-gapped hosts

`portcullis-v<version>-airgap.tar.gz` holds everything an offline machine
needs: the container image as a `docker load` tarball, the static binary, the
SBOM, the documentation, and a `SHA256SUMS` covering all of it. `INSTALL.md`
inside the bundle walks through verifying, loading and first run without
network access at any step.

## The container image

About 6 MB on `distroless/static`. No shell, no package manager, no libc, and
it runs as uid 65532 (`nonroot`). There is nothing in it to exec into, which is
the point.

```sh
docker run --rm   --read-only   -v /srv/portcullis:/var/lib/portcullis   -v /etc/portcullis:/etc/portcullis:ro   -e DATABASE_URL   -p 8080:8080   ghcr.io/sunnyallana/portcullis:0.2.0   serve --http 0.0.0.0:8080 --config /etc/portcullis/portcullis.toml
```

Three things about that command matter.

**The state volume must be durable.** `/var/lib/portcullis` holds the audit
chain and the approvals queue. On an ephemeral container filesystem you lose
pending approvals on every restart and the audit trail becomes worthless.

**The root filesystem can be read-only.** Nothing is written outside the
volume, so `--read-only` costs nothing and removes a class of problem.

**The image fails closed.** Serving on anything but loopback without an
`[auth]` block is refused outright:

```
error: configuration is invalid: refusing to serve 0.0.0.0:8080 with no
authentication; configure [auth] or bind 127.0.0.1
```

That is deliberate. A container published on a port with no authentication is
not a configuration anyone means to have, so the process will not start rather
than warn about it. Configure `[auth]` before you publish a port.

### Kubernetes

The probes need no credential, so wire them directly:

```yaml
livenessProbe:
  httpGet: { path: /healthz, port: 8080 }
readinessProbe:
  httpGet: { path: /readyz, port: 8080 }   # fails while the database is unreachable
securityContext:
  runAsNonRoot: true
  readOnlyRootFilesystem: true
  allowPrivilegeEscalation: false
  capabilities: { drop: ["ALL"] }
```

Mount the connection string from a Secret as `DATABASE_URL` and reference it
from the config as `dsn = "env:DATABASE_URL"`; the file itself then holds
nothing sensitive. Run **one replica** until shared limits land: rate limiting
and the idempotency store are per process, so a second replica enforces them
separately rather than jointly.

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

Give Portcullis its own database role rather than reusing an application login:

```sql
CREATE ROLE portcullis LOGIN PASSWORD '…';
GRANT USAGE ON SCHEMA public TO portcullis;
GRANT SELECT ON public.orders TO portcullis;
GRANT INSERT ON public.refunds TO portcullis;
```

Portcullis needs to read `information_schema` for its startup validation, which
every role can do by default.

### Checking it against a throwaway server first

`examples/postgres-schema.sql` is the demo data as real DDL, so you can try the
shipped actions against PostgreSQL before pointing anything at production:

```sh
docker run -d --name portcullis-pg -e POSTGRES_PASSWORD=portcullis-test \
           -e POSTGRES_DB=portcullis -p 55432:5432 postgres:18
psql "postgres://postgres:portcullis-test@localhost:55432/portcullis" \
     -f examples/postgres-schema.sql
```

Switch `[backend]` in a copy of `examples/orders.toml` to that DSN and run
`portcullis doctor`. The same container runs the backend integration tests:

```sh
PORTCULLIS_TEST_DATABASE_URL="postgres://postgres:portcullis-test@localhost:55432/portcullis" \
  cargo test -p portcullis-db --test postgres_live
```

Those tests are skipped when the variable is unset, so the ordinary
`cargo test` needs nothing installed.

## First run

```sh
portcullis doctor --config /etc/portcullis/orders.toml
```

`doctor` checks the file, connects, counts visible tables, validates every
action, reports warnings, and verifies the audit chain. It exits non-zero when
something needs attention, so it belongs in a deploy pipeline. It also flags a
`mask_salt` left at the example value and an audit log with fsync disabled.

Then:

```sh
portcullis validate    # actions against the live schema
portcullis tools       # exactly what an MCP client will see
```

## Pointing at MySQL

```toml
[backend]
kind = "mysql"
dsn = "env:DATABASE_URL"
max_connections = 10
```

`examples/orders-mysql.toml` and `examples/mysql-schema.sql` are the same demo
against MySQL. Two differences to know: model UUID columns as `CHAR(36)` and
declare the parameter `text`, and be aware that an action with `returning`
costs a second round trip, because MySQL has no `RETURNING` and the backend
re-selects the row on the same connection.

Grants, as for PostgreSQL:

```sql
CREATE USER 'portcullis'@'%' IDENTIFIED BY '…';
GRANT SELECT ON app.orders TO 'portcullis'@'%';
GRANT INSERT ON app.refunds TO 'portcullis'@'%';
```

## Serving over stdio

```sh
portcullis serve --config /etc/portcullis/orders.toml --role support_eu
```

stdout carries the protocol and nothing else. Logs go to stderr; set the level
with `PORTCULLIS_LOG` (`PORTCULLIS_LOG=info`, or `PORTCULLIS_LOG=portcullis_engine=debug`).

One process per role. The role fixes the caller's attributes for the lifetime
of the process, which is what scopes every call — see
[security-model.md](security-model.md#identity).

Under systemd:

```ini
[Service]
ExecStart=/usr/local/bin/portcullis serve --config /etc/portcullis/orders.toml --role support_eu
Environment=DATABASE_URL=postgres://portcullis@db/app?sslmode=verify-full
Environment=PORTCULLIS_LOG=info
WorkingDirectory=/var/lib/portcullis
User=portcullis
```

MCP clients launch the binary themselves, so for Claude Code:

```sh
claude mcp add orders -- portcullis serve --config /etc/portcullis/orders.toml --role support_eu
```

## Serving over HTTP

```sh
portcullis serve --http                      # uses [http] listen
portcullis serve --http 0.0.0.0:8080         # or override it
```

| Path | Auth | Purpose |
|---|---|---|
| `POST /mcp` | yes | MCP, one JSON-RPC request per POST |
| `GET /api/approvals` | yes | Parked calls, with a `decidable` flag per viewer |
| `POST /api/approvals/{id}/approve` | yes | Release one |
| `POST /api/approvals/{id}/deny` | yes | Refuse one |
| `GET /healthz` | no | Process is up |
| `GET /readyz` | no | Backend answers |
| `GET /metrics` | no | Prometheus counters and a latency histogram |
| `GET /` | page only | Approvals console |

There is no SSE stream and no batching: a `GET /mcp` and a JSON array both come
back with a message saying so. Notifications return `202` with no body.

TLS belongs in front. A minimal nginx location:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Authorization $http_authorization;
}
```

Keep `/metrics` off the public interface; it exposes action names and call
volumes.

### Identity

```toml
[auth]
kind = "oidc"
issuer = "https://id.example.com/"
audience = ["portcullis"]
role_claim = "portcullis_role"
attribute_claims = { region = "region" }
role_map = { "support-eu" = "support_eu" }
```

Keys load at startup, so a wrong `issuer` or `jwks_url` fails the boot. Check
it with `portcullis serve --http` and watch stderr: it logs how many keys it
loaded.

For machine callers:

```sh
portcullis apikey --role batch --caller nightly-reconcile
```

That prints the key once and the `[[auth.key]]` block to paste. Only the digest
goes in the file.

### The console

`GET /` serves a single page that lists parked calls and approves or denies
them. It asks for a bearer token, keeps it in `sessionStorage` for that tab
only, and sends it as a header; nothing is embedded in the page. Buttons are
disabled for requests the viewer may not decide, which the API reports per row
rather than letting the click fail.

## Environment variables

| Variable | Effect |
|---|---|
| `PORTCULLIS_CONFIG` | Default `--config` path |
| `PORTCULLIS_ROLE` | Default `--role` |
| `PORTCULLIS_CALLER` | Default caller identity in the audit log |
| `PORTCULLIS_LOG` | Log filter (`error`, `warn`, `info`, `debug`, or per-module) |

Secrets belong in `env:NAME` or `file:/path` references inside the config, not
in the file itself.

## The approval queue

```sh
portcullis approvals list
portcullis approvals approve apr_c31736cb8b --caller manager-jane
portcullis approvals deny    apr_c31736cb8b --caller manager-jane
```

Approving executes the call immediately, re-validating its arguments and
re-checking the caller's role first. A request can be claimed once; a second
attempt fails. Requests expire after `[approvals] ttl_secs`.

The queue is a JSONL event log, so `tail -f portcullis-approvals.jsonl` is a
perfectly good pager for a small team, and piping it into Slack is a few lines
of shell. A console is on the roadmap.

## The audit log

```sh
portcullis audit tail -n 50
portcullis audit verify
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

## Proving a change did not change behaviour

```sh
portcullis replay --record before.json        # today's answers
# … edit the configuration …
portcullis replay --against before.json       # exits 3 if anything differs
```

Replay takes the read calls out of the audit log, de-duplicates them, and runs
each one again as the role and scope that made it. Writes are never replayed.
Calls whose arguments were masked are skipped, because the log holds the masked
form. The report names each difference as a row-count change, a content change
at the same count, or a call that started or stopped failing.

## Limits and tuning

- `[limits] max_rows` caps every action. An action asking for more is capped
  and warned about at startup.
- `timeout` per action is a client-side deadline; `statement_timeout_secs` is
  the server-side backstop. Set the backstop above the largest action timeout.
- `rate_limit` is per caller and action, in a one-minute sliding window, held
  in memory. It resets on restart and is not shared between processes, so a
  horizontally scaled deployment enforces it per replica.
- The idempotency store is a local file for the same reason. Two replicas do
  not share replay protection.
- Reads fetch one row beyond the limit to detect truncation, and tell the model
  when a result was cut short.

## Upgrading

Action definitions are validated against the live schema at every startup, so a
schema migration that removes a column Portcullis uses will fail the next start
rather than fail a tool call. Run `portcullis validate` against the new schema
before the migration goes out.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `table X does not exist` at startup | Wrong schema, or the role cannot see it. Check `[backend] schemas` and grants |
| `X exists in more than one schema` | Qualify it: `public.orders` |
| `row_filter needs $caller.region, but role Y does not define it` | Add the attribute to that role, or remove it from its `allow` list |
| Every call returns no rows | The role's attributes do not match any data. Check with `portcullis call --role …` |
| `no database connection was free within the pool timeout` | Raise `max_connections`, or find the slow action with `portcullis audit tail` |
| A column is missing from `returns` validation | Its type is not modelled (arrays, ranges and custom types on PostgreSQL; blobs and spatial types on MySQL) |
| `401` with `WWW-Authenticate: Bearer` | No token, or one this server will not accept. `portcullis_auth_failures_total` says which |
| `403` on every HTTP call | The token's role claim maps to nothing this deployment defines. Check `role_map` |
| `403` when approving | `approver_roles`, or the requester trying to release their own request |
| MySQL: "cannot return them" | The action asks for `returning` from a table with no primary key the write supplies |
