#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

# Synthetic test signals only; no recordings or copyrighted music.
# FFmpeg 8.1.2 with libmp3lame and libvorbis was used for the checked-in files.
encoder="${FFMPEG:-ffmpeg}"
generate() {
  local destination="$1"
  local duration="${FIXTURE_DURATION:-0.6}"
  local rate="${FIXTURE_RATE:-22050}"
  shift
  "$encoder" -hide_banner -loglevel error -y \
    -f lavfi -i "aevalsrc=0.25*sin(2*PI*440*t)|0.2*sin(2*PI*660*t):s=$rate:d=$duration" \
    -threads 1 -fflags +bitexact -flags:a +bitexact \
    -metadata title='Cadence synthetic tone' \
    -metadata artist='Cadence tests' -metadata album='Decoder fixtures' \
    "$@" "$destination"
}

generate tone-mono.mp3 -ac 1 -c:a libmp3lame -b:a 48k -id3v2_version 3
generate tone.flac -c:a flac -sample_fmt s16 -compression_level 8
generate tone-order.flac -c:a flac -sample_fmt s16 -compression_level 8 -metadata track=7 -metadata disc=2 -metadata album_artist='Cadence album ensemble'
generate tone.ogg -c:a libvorbis -q:a 3
generate tone.aac -c:a aac -b:a 64k -f adts
generate tone-aac.m4a -c:a aac -b:a 64k
generate tone-alac.m4a -c:a alac -sample_fmt s16p
generate tone.aiff -c:a pcm_s16be -write_id3v2 1
generate tone.caf -c:a pcm_s16le
FIXTURE_DURATION=10 FIXTURE_RATE=44100 generate tone-long.ogg -c:a libvorbis -q:a 3
