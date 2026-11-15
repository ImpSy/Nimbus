# Reproducible Linux / Steam Deck build environment.
#
# Ubuntu 22.04 (glibc 2.35) so the binary runs on SteamOS 3.x and most
# distributions. Toolchains come from the repo's mise.toml (same versions as
# on a dev machine); FFmpeg and SDL3 are built from source and linked
# statically. The resulting binary only needs glibc, libva/libdrm (present on
# SteamOS) and the X11/Wayland/PipeWire libraries SDL loads at runtime.
#
# Used through mise:  mise run deck:build   /   mise run deck:e2e
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl git pkg-config nasm xz-utils \
      clang libclang-dev file \
      libva-dev libdrm-dev \
      libx11-dev libxext-dev libxrandr-dev libxcursor-dev libxi-dev libxss-dev libxfixes-dev \
      libxtst-dev libxrender-dev libgbm-dev libusb-1.0-0-dev \
      libxkbcommon-dev libwayland-dev wayland-protocols libegl-dev libgl-dev libvulkan-dev \
      libdecor-0-dev libudev-dev libdbus-1-dev libibus-1.0-dev \
      libasound2-dev libpulse-dev libpipewire-0.3-dev \
      xvfb mesa-vulkan-drivers libvulkan1 \
    && rm -rf /var/lib/apt/lists/*

# mise manages Rust, cmake, … exactly like on a developer machine.
RUN curl -fsSL https://mise.run | MISE_INSTALL_PATH=/usr/local/bin/mise sh
ENV MISE_YES=1 \
    MISE_TRUSTED_CONFIG_PATHS=/src \
    MISE_EXPERIMENTAL=1 \
    CARGO_TARGET_DIR=/build/target \
    PATH=/root/.local/share/mise/shims:$PATH

WORKDIR /src
CMD ["bash", "packaging/linux/build.sh"]
