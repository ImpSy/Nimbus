# Reproducible Linux / Steam Deck build environment.
#
# Ubuntu 22.04 (glibc 2.35) so the binary runs on SteamOS 3.x and most
# distributions. FFmpeg is built from source and linked statically (VA-API
# enabled); SDL3 is built from source and linked statically. The resulting
# binary only needs glibc, libva/libdrm (present on SteamOS) and the usual
# X11/Wayland/PipeWire libraries that SDL loads at runtime.
#
#   docker build --platform linux/amd64 -t nimbus-build packaging/linux
#   docker run --rm --platform linux/amd64 -v "$PWD":/src -v nimbus-cargo:/root/.cargo/registry \
#       -v nimbus-target:/src/target nimbus-build
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl git pkg-config cmake ninja-build nasm yasm \
      clang libclang-dev file \
      libva-dev libdrm-dev \
      libx11-dev libxext-dev libxrandr-dev libxcursor-dev libxi-dev libxss-dev libxfixes-dev \
      libxtst-dev libxrender-dev libgbm-dev libusb-1.0-0-dev \
      libxkbcommon-dev libwayland-dev wayland-protocols libegl-dev libgl-dev libvulkan-dev \
      libdecor-0-dev libudev-dev libdbus-1-dev libibus-1.0-dev \
      libasound2-dev libpulse-dev libpipewire-0.3-dev \
      fuse libfuse2 desktop-file-utils \
    && rm -rf /var/lib/apt/lists/*

ARG RUST_VERSION=1.100.0
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain ${RUST_VERSION}
ENV PATH=/root/.cargo/bin:$PATH

WORKDIR /src
CMD ["bash", "packaging/linux/build.sh"]
