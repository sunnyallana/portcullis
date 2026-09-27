# Contributing

## Naming: never type the product name in code

Every place code has to know what the product is called reads it from
`portcullis-core::branding`. The one rule: if you are about to type
`portcullis`, `PORTCULLIS_` or `Portcullis` inside a `.rs` file, import the
constant instead.

```rust
use portcullis_core::branding;

println!("run `{} doctor` for a fuller check", branding::BIN);
```

The exception is a `#[cfg(test)]` block. A test that builds its expected string
from the same constant as the code under test only restates the code; assert
the concrete string you expect to see.

### Adding an environment variable

```rust
// crates/portcullis-core/src/branding.rs
pub mod env {
    pub const TIMEOUT: &str = "PORTCULLIS_TIMEOUT";   // 1. declare it
    pub const ALL: &[&str] = &[CONFIG, ROLE, CALLER, LOG, TIMEOUT];   // 2. list it
}
```

```rust
#[arg(long, env = env_var::TIMEOUT)]   // 3. use it
```

`every_environment_variable_carries_the_prefix` fails if you forget the prefix.
Leaving it out of `ALL` is the one mistake nothing catches, so do step 2 first.

### Adding a metric series

Declare it in `branding::metrics`, add it to `metrics::ALL`, then render both
the header and the samples from the constant:

```rust
header(&mut out, series::CACHE_HITS, "counter", "Cache hits by action.");
let _ = writeln!(out, "{}{{action=\"{}\"}} {n}", series::CACHE_HITS, escape(action));
```

`the_rendered_output_names_only_declared_series` reads the exposition back and
rejects any name that is not in `ALL`. That test exists because the header
lines were once literals while the samples used constants, which would have
emitted a metric family whose comments and samples disagreed — something
Prometheus rejects and nothing else would have noticed.

### What does not belong in branding

Operator-facing identity. The name an MCP client sees comes from `[server]
name` in the deployment's own configuration, not from the product name, and
should stay that way.

Crate names. `portcullis_core` is a compile-time identifier; no constant can
rename it. A rebrand renames the crates mechanically.

## Renaming the product

Four steps, and one decision in the middle that is not mechanical.

**1. Move the crates.** Use `git mv` so history follows:

```sh
for c in core sql db engine mcp http cli; do
  git mv "crates/portcullis-$c" "crates/newname-$c"
done
```

**2. Rewrite the text.** Across `*.rs`, `*.toml`, `*.md`, `*.html`, `*.yml`,
`*.json`, `*.sql`, in this order so the specific spellings go first:
`PORTCULLIS_` → `NEWNAME_`, `portcullis_` → `newname_`, `portcullis-` →
`newname-`, `Portcullis` → `Newname`, `portcullis` → `newname`.

**3. Decide about the frozen names.** This is the part that is not a
find-and-replace.

`ENV_PREFIX`, `METRIC_PREFIX`, everything in `env::` and everything in
`metrics::` are marked frozen. They are not the product's name any more; they
are an interface living in other people's systemd units, Helm charts, Grafana
dashboards and alert rules. Renaming them silently makes every dashboard go
blank and every deployment stop reading its configuration, with no error
anywhere.

- **Before you have users:** move them with everything else.
- **After you have users:** leave them. A deployment running the renamed binary
  should keep reading `PORTCULLIS_CONFIG` and keep exposing
  `portcullis_tool_calls_total`. When you do want to move them, read both names
  for at least one release, prefer the new one, warn when the old one is used,
  and say so in the changelog.

`the_prefixes_agree_with_the_binary_name_today` will fail the moment you change
`BIN` without touching the prefixes. That failure is the prompt, not a bug:
delete the test or update it, deliberately, once you have made the decision
above.

**4. Check your work.**

```sh
cargo test --workspace --all-features
grep -ri oldname --exclude-dir=target --exclude-dir=.git .
```

The grep should return nothing. Then rename the repository directory, and
remember that trademark and domain matter more than crates.io availability.

## Before you open a pull request

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
cargo deny check
```

The database backends have integration tests that skip unless a server is
configured; the README has the `docker run` lines for all three. CI runs them
against real PostgreSQL, MySQL and SQL Server, so a backend change that passes
locally without them has not been tested.

SQL Server is not a default feature and does not go through `sqlx`, so
`--all-features` is the only invocation that compiles it. A change to the
`Dialect` trait or to `portcullis-sql::write` that builds without it may still
be broken there.

## Adding a test that catches a regression

Prove it. Reintroduce the bug, watch the new test fail with a message that
names the problem, then restore the fix. A test written after the fix that has
never failed is a test that happens to pass.
