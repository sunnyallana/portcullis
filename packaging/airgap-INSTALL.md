# Portcullis — offline install

Everything needed to run Portcullis on a machine with no internet access. No
package manager, no registry pull, no build step.

## What is in this bundle

| File | What it is |
|---|---|
| `portcullis-image.tar` | The container image, ready for `docker load` or `podman load` |
| `portcullis` | The same binary, statically linked against musl. No runtime dependencies |
| `portcullis.cdx.json` | CycloneDX SBOM: every dependency, version and licence |
| `SHA256SUMS` | Checksums for everything above |
| `docs/`, `examples/`, `README.md`, `CHANGELOG.md` | The documentation that matches this build |

## 1. Verify before you run

```sh
sha256sum -c SHA256SUMS
```

The release these files came from is also signed. On a machine that does have
network access:

```sh
# the container image
cosign verify ghcr.io/sunnyallana/portcullis:<version> \
  --certificate-identity-regexp '^https://github\.com/sunnyallana/portcullis/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# the release artifacts, including this bundle
gh attestation verify portcullis-<version>-airgap.tar.gz --repo sunnyallana/portcullis
```

Both prove the artifact was built by the release workflow in that repository,
from that commit, rather than assembled by someone else. There is no signing
key to manage: the identity is the workflow itself.

## 2. Install

Either the image:

```sh
docker load -i portcullis-image.tar
docker image ls | grep portcullis
```

or the binary:

```sh
install -m 0755 portcullis /usr/local/bin/portcullis
portcullis --version
```

## 3. First run, with no database

```sh
portcullis init /var/lib/portcullis
portcullis validate --config /var/lib/portcullis/portcullis.toml
portcullis call --config /var/lib/portcullis/portcullis.toml \
  --action find_order --arg order_no=8812 --role support_eu --caller you
```

That runs against a bundled in-memory fixture, so it proves the binary works
before anything touches your data.

## 4. Point it at your database

```sh
portcullis profile --dsn "env:DATABASE_URL" --out draft.toml
```

Read the draft. It only ever proposes read actions, the masks are suggestions
from column names and sampled values, and any row filter it spots is written
commented out because only you know which attribute your callers carry. Edit
it down, then:

```sh
portcullis doctor --config draft.toml
```

`doctor` checks the file, connects, validates every action against the live
schema and verifies the audit chain. It exits non-zero when something needs
attention, so it belongs in your deployment pipeline.

## 5. Running it

Read `docs/operations.md` before serving anything. The two points that catch
people out:

- **The state directory must be durable.** `/var/lib/portcullis` holds the
  audit chain and the approvals queue. On an ephemeral container filesystem
  you lose pending approvals on every restart.
- **It refuses to serve a non-loopback address without an `[auth]` block.**
  That is deliberate, not a bug. Configure authentication before you publish a
  port.

Run one replica. Rate limiting and replay protection are per process, so a
second replica enforces them separately rather than jointly.

## Support

`docs/security-model.md` is the document to hand your security reviewer: the
threat model, how each control is enforced, and an explicit list of what this
system does not do.
