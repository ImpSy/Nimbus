#!/usr/bin/env bash
# Generate the test media used by the mock xCloud server (needs ffmpeg with
# libx264 + libopus). Usage: scripts/mock-media.sh [out_dir]
set -euo pipefail
OUT="${1:-target/mock-media}"
mkdir -p "$OUT"
if [ ! -f "$OUT/video.h264" ]; then
  ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=1920x1080:rate=60" -t 10 \
    -vf "drawtext=text='Nimbus mock stream %{pts\:hms}':fontsize=64:fontcolor=white:box=1:boxcolor=black@0.6:x=60:y=60" \
    -c:v libx264 -profile:v main -preset veryfast -tune zerolatency \
    -b:v 15M -maxrate 15M -bufsize 1M \
    -x264-params "aud=1:keyint=120:bframes=0:repeat-headers=1" \
    -pix_fmt yuv420p -f h264 "$OUT/video.h264" 2>/dev/null || \
  ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=1920x1080:rate=60" -t 10 \
    -c:v libx264 -profile:v main -preset veryfast -tune zerolatency \
    -b:v 15M -maxrate 15M -bufsize 1M \
    -x264-params "aud=1:keyint=120:bframes=0:repeat-headers=1" \
    -pix_fmt yuv420p -f h264 "$OUT/video.h264"
fi
if [ ! -f "$OUT/audio.opus" ]; then
  ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "sine=frequency=330:duration=10" -ac 2 -c:a libopus -b:a 96k \
    -frame_duration 20 -f ogg "$OUT/audio.opus"
fi
echo "$OUT"
