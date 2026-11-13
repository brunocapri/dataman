# The Linux release build: Debian 12 (glibc 2.36) with what GPUI links and the packaging tools.
# RUST_TOOLCHAIN is rust-toolchain.toml's channel (scripts/linux/build.sh passes it).
ARG RUST_TOOLCHAIN
FROM rust:${RUST_TOOLCHAIN}-bookworm
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    clang lld cmake pkg-config file dpkg-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
    libfontconfig-dev libfreetype-dev libvulkan1 libegl1 \
    && rm -rf /var/lib/apt/lists/*
