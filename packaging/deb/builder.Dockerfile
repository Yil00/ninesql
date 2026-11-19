# Build environment of the NineSQL .deb (P6-A, ADR-0010 §1 and §5).
#
# Ubuntu 22.04 has the oldest glibc of the release targets (2.35; Debian 12 has 2.36), so a
# binary linked here runs on every distribution of docs/10-supported-platforms.md. A binary
# linked on a newer system (Ubuntu 24.04: glibc 2.39) would not start on Debian 12 or
# Ubuntu 22.04.
#
# The image holds no Rust toolchain and no crate: build-in-container.sh mounts the host's
# rustup toolchain and cargo registry read-write caches and runs cargo with `--offline`
# under `--network none`, so the build itself never touches the network.
#
# The base image is pinned by digest (the multi-arch index of ubuntu:22.04). Override BASE to
# pull the same digest from a mirror, e.g.
#   --build-arg BASE=mirror.gcr.io/library/ubuntu@sha256:<same digest>
# Follow-up (docs, packaging/README.md): pin the apt packages too, through snapshot.ubuntu.com,
# so that a third party rebuilding a tag months later gets the same linker and libraries.
ARG BASE=docker.io/library/ubuntu@sha256:9abcaffc92a5279fff3a6a41635c0800ea33e528425fb389658806476e7defde
FROM ${BASE}

# The system packages of docs/10-supported-platforms.md (the same list as the CI "rust" job).
# The binary links libxcb, libxcb-xkb, libxkbcommon and libxkbcommon-x11 only; Vulkan,
# Wayland and fontconfig are loaded at run time (P0-A spike).
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      build-essential pkg-config clang cmake libvulkan-dev libwayland-dev \
      libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev \
      libfontconfig-dev libfreetype-dev libzstd-dev libasound2-dev \
 && rm -rf /var/lib/apt/lists/*
