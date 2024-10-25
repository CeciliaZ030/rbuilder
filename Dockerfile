#
# Base container (with sccache and cargo-chef)
#
# - https://github.com/mozilla/sccache
# - https://github.com/LukeMathWalker/cargo-chef
#
# Based on https://depot.dev/blog/rust-dockerfile-best-practices
#
FROM rust:1.81 as base

ARG FEATURES

RUN cargo install sccache --version ^0.8
RUN cargo install cargo-chef --version ^0.1

RUN apt-get update \
    && apt-get install -y clang libclang-dev

ENV CARGO_HOME=/usr/local/cargo
ENV RUSTC_WRAPPER=sccache
ENV SCCACHE_DIR=/sccache

#
# Planner container (running "cargo chef prepare")
#
FROM base AS planner
WORKDIR /app

COPY ./rbuilder/Cargo.lock ./Cargo.lock
COPY ./rbuilder/Cargo.toml ./Cargo.toml
COPY ./rbuilder/crates/ ./crates/

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo chef prepare --recipe-path recipe.json

#
# Builder container (running "cargo chef cook" and "cargo build --release")
#
FROM base as builder

COPY --from=planner /app/recipe.json /app/rbuilder/recipe.json
COPY ./rbuilder/Cargo.lock /app/rbuilder/Cargo.lock
COPY ./rbuilder/Cargo.toml /app/rbuilder/Cargo.toml
COPY ./rbuilder/crates/ /app/rbuilder/crates/
COPY ./reth /app/reth
COPY ./revm /app/revm
COPY ./revm-inspectors /app/revm-inspectors
RUN ls
WORKDIR /app/rbuilder
RUN pwd && ls
RUN --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json


RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo build --release --features="$FEATURES"

#
# Runtime container
#
FROM ubuntu:22.04 AS runtime

WORKDIR /app

COPY --from=builder /app/rbuilder/target/release/rbuilder /app/rbuilder

ENTRYPOINT ["/app/rbuilder"]
