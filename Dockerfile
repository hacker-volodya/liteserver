FROM rust:1.77.2-bookworm AS builder
WORKDIR /app
RUN apt update && apt install -y cmake libclang-dev clang build-essential libsodium-dev libsecp256k1-dev lz4 liblz4-dev libssl-dev zlib1g-dev libreadline-dev libssl3
COPY . .
RUN --mount=type=cache,target=/var/cache/buildkit \
    CARGO_HOME=/var/cache/buildkit/cargo \
    CARGO_TARGET_DIR=/var/cache/buildkit/target \
    cargo build --release --locked && \
    cp /var/cache/buildkit/target/release/ton-node /

FROM debian:bookworm-slim AS runtime
RUN apt update && apt install -y lz4 libssl3 libsodium-dev libsecp256k1-dev
WORKDIR /app
COPY --from=builder /ton-node /usr/local/bin
VOLUME /data
ENTRYPOINT ["/usr/local/bin/ton-node"]

# http
EXPOSE 3000/tcp

# liteapi
EXPOSE 3333/tcp

# nodes p2p
EXPOSE 30303/udp
