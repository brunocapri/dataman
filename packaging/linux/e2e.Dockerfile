# The Linux CI's test machine (.github/workflows/linux.yml) for scripts/linux/e2e.sh: Ubuntu 22.04,
# the workflow's packages, rust-toolchain.toml's Rust, and docker with compose (the host's
# daemon, by socket).
FROM docker:29-cli AS docker
FROM docker/compose-bin:v2.29.7 AS compose
FROM ubuntu:22.04
ARG RUST_TOOLCHAIN
ARG DEPS
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git build-essential clang cmake pkg-config python3 openssh-client $DEPS \
    && rm -rf /var/lib/apt/lists/*
COPY --from=docker /usr/local/bin/docker /usr/local/bin/docker
COPY --from=compose /docker-compose /usr/libexec/docker/cli-plugins/docker-compose
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain "$RUST_TOOLCHAIN"
