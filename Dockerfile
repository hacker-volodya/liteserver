FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder 
RUN apt update && apt install -y libclang-dev
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
WORKDIR /app
COPY --from=builder /app/target/release/ton-node /usr/local/bin
COPY config /config
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/ton-node"]