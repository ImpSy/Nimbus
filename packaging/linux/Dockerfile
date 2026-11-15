# Reproducible Linux / Steam Deck build environment.
#
# Ubuntu 22.04 (glibc 2.35) so the binary runs on SteamOS 3.x and most
# distributions. Toolchains come from the repo's mise.toml (same versions as
# on a dev machine); FFmpeg (H.264 + VA-API) is built from source and linked
# statically. The resulting binary needs glibc, libva/libdrm, libudev and
# libasound (all present on SteamOS); the X11 libraries are loaded at runtime.
#
# Used through mise:  mise run deck:build   /   mise run deck:e2e
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl git pkg-config nasm xz-utils \
      clang libclang-dev file \
      libva-dev libdrm-dev libudev-dev libasound2-dev \
      xvfb mesa-vulkan-drivers libvulkan1 \
      libx11-6 libx11-xcb1 libxcursor1 libxrandr2 libxi6 libxkbcommon0 libxkbcommon-x11-0 \
    && rm -rf /var/lib/apt/lists/*

# mise manages Rust and the test-media tools exactly like on a developer machine.
RUN curl -fsSL https://mise.run | MISE_INSTALL_PATH=/usr/local/bin/mise sh
ENV MISE_YES=1 \
    MISE_TRUSTED_CONFIG_PATHS=/src \
    MISE_EXPERIMENTAL=1 \
    CARGO_TARGET_DIR=/build/target \
    PATH=/root/.local/share/mise/shims:$PATH

WORKDIR /src
CMD ["bash", "packaging/linux/build.sh"]
