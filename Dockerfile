# Build: docker build --network=host -t tor-js-gateway .
FROM rust:1.89-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libsqlite3-0 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/tor-js-gateway /usr/local/bin/

# The KPS listener is UDP: one port carries both QUIC and WebRTC.
EXPOSE 42298/udp

ENTRYPOINT ["tor-js-gateway"]
