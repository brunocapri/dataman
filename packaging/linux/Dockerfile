# The Linux release build: Debian 12 (glibc 2.36) with what GPUI links and the packaging tools.
FROM rust:1-bookworm
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    clang lld cmake pkg-config file imagemagick dpkg-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
    libfontconfig-dev libfreetype-dev libvulkan1 libegl1 \
    && rm -rf /var/lib/apt/lists/*
