# syntax=docker/dockerfile:1
#
# Builds one Spuria server binary into a slim runtime image.
# Select which with --build-arg BINARY=spuria-signaling | spuria-relay.

FROM rust:1-bookworm AS builder
WORKDIR /build

# Cache dependency compilation: copy manifests first, then sources.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

ARG BINARY=spuria-signaling
RUN cargo build --release --bin "${BINARY}" \
    && cp "target/release/${BINARY}" /usr/local/bin/spuria-app

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home spuria
COPY --from=builder /usr/local/bin/spuria-app /usr/local/bin/spuria-app
USER spuria
# Ports are configured via flags/env; document the defaults.
# signaling: 21116/tcp (ws), 21117/udp (srflx), 8088/tcp (admin, optional)
# relay:     21118/tcp
EXPOSE 21116/tcp 21117/udp 21118/tcp 8088/tcp
ENTRYPOINT ["/usr/local/bin/spuria-app"]
