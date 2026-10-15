# syntax=docker/dockerfile:1
# The released image: the gateway, worker and observer, the operator tools
# (admin, contract, testnet), the seller's walkthrough (try), and the vault's
# Wasm, built from the same commit so a deployment can verify the code it
# installs. Runs as an unprivileged user; secrets arrive as mounted files.
# Base images are pinned by digest, so a rebuild of the same commit starts
# from the same layers.

FROM rust:1.96-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS build
WORKDIR /src
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang libdbus-1-3 \
    && rm -rf /var/lib/apt/lists/*
# The same pinned stellar CLI the contract CI job builds the Wasm with, so the
# image's Wasm hashes to the one CI reports for the commit.
ARG STELLAR_CLI_VERSION=28.1.0
ARG STELLAR_CLI_SHA256=c1680deee94301d33ada7a17f98411e642a4248c727afbd2e43050d345746462
RUN curl -fsSL --retry 5 --retry-delay 2 -o /tmp/stellar.tar.gz \
        "https://github.com/stellar/stellar-cli/releases/download/v${STELLAR_CLI_VERSION}/stellar-cli-${STELLAR_CLI_VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
    && echo "${STELLAR_CLI_SHA256}  /tmp/stellar.tar.gz" | sha256sum --check --strict \
    && tar -xzf /tmp/stellar.tar.gz -C /usr/local/bin stellar \
    && rm /tmp/stellar.tar.gz
COPY . .
# Queries are checked against the committed .sqlx cache, not a live database.
ENV SQLX_OFFLINE=true
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
        -p fermah-pay-stellar-gateway -p fermah-pay-stellar-cli \
        --bin fermah-pay-stellar-gateway --bin fermah-pay-stellar-worker \
        --bin fermah-pay-stellar-observer --bin fermah-pay-stellar-admin \
        --bin fermah-pay-stellar-contract --bin fermah-pay-stellar-testnet \
        --bin fermah-pay-stellar-try \
    && stellar contract build --package fermah-pay-stellar-vault --profile contract --locked \
        --out-dir /out/wasm \
    && mkdir -p /out/bin \
    && for bin in gateway worker observer admin contract testnet try; do \
        cp "target/release/fermah-pay-stellar-$bin" /out/bin/; done

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/bin/ /usr/local/bin/
COPY --from=build /out/wasm/fermah_pay_stellar_vault.wasm /usr/local/share/fermah-pay-stellar/
USER 65534:65534
ENTRYPOINT ["fermah-pay-stellar-try"]
