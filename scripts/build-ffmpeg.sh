#!/usr/bin/env bash
# Build the minimal static FFmpeg Nimbus links against, into $FFMPEG_DIR.
#
# Only what the client needs: H.264 / HEVC / Opus decoders, libswresample
# (required by the Opus decoder) and the platform hardware decoder
# (VideoToolbox on macOS, VA-API on Linux). Takes ~1 minute, runs once per
# clone/platform; `mise install` / `mise run setup` call it automatically.
set -euo pipefail

VERSION=9.0.2
SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
# Bump when the configure flags below change, to force a rebuild.
CONFIG_REV=1

PREFIX="${FFMPEG_DIR:?FFMPEG_DIR is not set - run this through mise (mise run ffmpeg)}"
STAMP="$PREFIX/.nimbus-ffmpeg"
WANT="$VERSION-$CONFIG_REV"
if [ -f "$PREFIX/lib/libavcodec.a" ] && [ "$(cat "$STAMP" 2>/dev/null)" = "$WANT" ]; then
  echo "FFmpeg $VERSION already built in $PREFIX"
  exit 0
fi

WORK="$(dirname "$PREFIX")/src"
TARBALL="$WORK/ffmpeg-$VERSION.tar.xz"
mkdir -p "$WORK"
if [ ! -f "$TARBALL" ]; then
  echo "Downloading FFmpeg ${VERSION}..."
  curl -fsSL --retry 3 -o "$TARBALL.part" "https://ffmpeg.org/releases/ffmpeg-$VERSION.tar.xz"
  mv "$TARBALL.part" "$TARBALL"
fi
if command -v sha256sum >/dev/null; then
  GOT=$(sha256sum "$TARBALL" | cut -d' ' -f1)
else
  GOT=$(shasum -a 256 "$TARBALL" | cut -d' ' -f1)
fi
if [ "$GOT" != "$SHA256" ]; then
  echo "Checksum mismatch for $TARBALL (got $GOT)" >&2
  rm -f "$TARBALL"
  exit 1
fi

BUILD="$WORK/ffmpeg-$VERSION-$(uname -s)-$(uname -m)"
rm -rf "$BUILD" "$PREFIX"
mkdir -p "$BUILD"
tar -xJf "$TARBALL" -C "$BUILD" --strip-components=1

ARGS=(
  --prefix="$PREFIX"
  --enable-static --disable-shared --enable-pic
  --disable-programs --disable-doc --disable-debug --disable-network
  --disable-autodetect --disable-everything
  --disable-avdevice --disable-avformat --disable-avfilter --disable-swscale
  --enable-avcodec --enable-avutil --enable-swresample --enable-pthreads
  --enable-decoder=h264,hevc,opus
  --enable-parser=h264,hevc,opus
)

case "$(uname -s)" in
  Darwin)
    ARGS+=(
      --enable-videotoolbox
      --enable-hwaccel=h264_videotoolbox,hevc_videotoolbox
      --extra-cflags=-mmacosx-version-min=11.0
      --extra-ldflags=-mmacosx-version-min=11.0
    )
    ;;
  Linux)
    if pkg-config --exists libva libva-drm libdrm 2>/dev/null; then
      ARGS+=(--enable-vaapi --enable-libdrm --enable-hwaccel=h264_vaapi,hevc_vaapi)
    else
      echo "warning: libva/libdrm development files not found; building without VA-API" >&2
      echo "         (Debian/Ubuntu: apt install libva-dev libdrm-dev)" >&2
    fi
    ;;
esac

case "$(uname -m)" in
  x86_64 | amd64 | i?86)
    if ! command -v nasm >/dev/null; then
      echo "warning: nasm not found; building without x86 assembly (slower software decode)" >&2
      ARGS+=(--disable-x86asm)
    fi
    ;;
esac

JOBS=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
echo "Configuring FFmpeg ${VERSION} -> ${PREFIX}"
(cd "$BUILD" && ./configure "${ARGS[@]}" >"$BUILD/configure.out" 2>&1) || {
  tail -40 "$BUILD/configure.out" >&2
  tail -40 "$BUILD/ffbuild/config.log" >&2 || true
  exit 1
}
echo "Building FFmpeg (${JOBS} jobs)..."
make -C "$BUILD" -j"$JOBS" >"$BUILD/make.out" 2>&1 || { tail -40 "$BUILD/make.out" >&2; exit 1; }
make -C "$BUILD" install >/dev/null
echo "$WANT" >"$STAMP"
rm -rf "$BUILD"
echo "FFmpeg $VERSION installed in $PREFIX"
