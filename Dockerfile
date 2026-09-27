# A statically linked binary on a base image with nothing else in it.
#
# Portcullis sits between an agent and a production database, so the image is
# the smallest attack surface that can still run: no shell, no package manager,
# no libc to patch. That is possible because every dependency is pure Rust or
# builds against musl — rustls rather than OpenSSL for TLS, and jsonwebtoken's
# rust_crypto provider rather than aws-lc-rs.
#
# There is no dependency-caching layer here on purpose. A release build happens
# once in CI, and local iteration should use cargo directly rather than Docker;
# cargo-chef would buy rebuild speed nobody in this workflow needs, at the cost
# of a file that is harder to read.

FROM rust:1.98-slim-bookworm AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /src
COPY . .

# --locked so the image is built from the committed Cargo.lock and not from
# whatever resolved today.
RUN cargo build --release --locked \
    --target x86_64-unknown-linux-musl \
    --bin portcullis \
    && strip target/x86_64-unknown-linux-musl/release/portcullis


FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=build /src/target/x86_64-unknown-linux-musl/release/portcullis /portcullis

# The audit log and the approvals queue live here. Mount a durable volume:
# parked approvals and the tamper-evident chain must survive a restart, and an
# ephemeral container filesystem loses both.
WORKDIR /var/lib/portcullis
VOLUME ["/var/lib/portcullis"]

# 65532 is distroless's `nonroot`. The process needs no privileges and should
# never be given any; run the container read-only apart from the volume above.
USER nonroot:nonroot

EXPOSE 8080

# No HEALTHCHECK: the image has no shell and no curl to run one with. Point
# your orchestrator's probes at the endpoints instead —
#   liveness:  GET /healthz   (the process is up)
#   readiness: GET /readyz    (the database answers)
# Neither requires a credential.

ENTRYPOINT ["/portcullis"]
CMD ["serve", "--http", "0.0.0.0:8080", "--config", "/etc/portcullis/portcullis.toml"]
