FROM lukemathwalker/cargo-chef:latest-rust-1.94.0-slim-bullseye AS chef
WORKDIR /app
RUN apt update && apt install -y ca-certificates pkg-config libssl-dev git openssh-client libclang-dev cmake g++ protobuf-compiler
RUN update-ca-certificates

FROM chef AS planner
COPY ./Cargo.lock ./Cargo.lock
COPY ./Cargo.toml ./Cargo.toml
COPY ./vzdv ./vzdv
COPY ./vzdv-bot ./vzdv-bot
COPY ./vzdv-site ./vzdv-site
COPY ./vzdv-tasks ./vzdv-tasks
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY ./Cargo.lock ./Cargo.lock
COPY ./Cargo.toml ./Cargo.toml
COPY ./vzdv ./vzdv
COPY ./vzdv-bot ./vzdv-bot
COPY ./vzdv-site ./vzdv-site
COPY ./vzdv-tasks ./vzdv-tasks
# Build all binaries for release
RUN cargo build --release --all-features

FROM debian:bullseye-slim AS runtime
WORKDIR /app
RUN apt update && apt install -y ca-certificates
RUN update-ca-certificates
COPY --from=builder /app/target/release/vzdv-site /app/vzdv-site
COPY --from=builder /app/target/release/vzdv-bot /app/vzdv-bot
COPY --from=builder /app/target/release/vzdv-tasks /app/vzdv-tasks
COPY ./static /app/static
# /assets will be a volume
