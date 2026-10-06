# syntax=docker/dockerfile:1
# The gateway, worker, observer and admin binaries, for the local
# development stack (deploy/dev/compose.yaml). Not a hardened production
# image: it also carries jq for the provisioning script.

FROM rust:1.96-bookworm AS build
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang \
    && rm -rf /var/lib/apt/lists/*
COPY . .
# Queries are checked against the committed .sqlx cache, not a live database.
ENV SQLX_OFFLINE=true
# Cargo features to build with, e.g. `aws-kms` for `aws-kms://` key
# references (compose passes DEV_CARGO_FEATURES).
ARG CARGO_FEATURES=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked ${CARGO_FEATURES:+--features "$CARGO_FEATURES"} \
        -p fermah-pay-stellar-gateway -p fermah-pay-stellar-cli \
        --bin fermah-pay-stellar-gateway --bin fermah-pay-stellar-worker \
        --bin fermah-pay-stellar-observer --bin fermah-pay-stellar-admin \
    && mkdir -p /out \
    && cp target/release/fermah-pay-stellar-gateway target/release/fermah-pay-stellar-worker \
          target/release/fermah-pay-stellar-observer target/release/fermah-pay-stellar-admin /out/

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates jq \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /usr/local/bin/
COPY deploy/dev/provision.sh /usr/local/bin/provision.sh
# A new named volume mounted here takes this mode, so the services, which run
# as the invoking user rather than root, can write the provisioning result.
RUN mkdir -p /state && chmod 1777 /state
# Compose runs the services as the invoking user; anything else gets an
# unprivileged one rather than root.
USER 65534:65534
