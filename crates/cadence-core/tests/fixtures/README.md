# Decoder fixtures

These are generated mathematical test tones, not recordings or music. The short
clips contain 0.6 seconds at 22,050 Hz: 440 Hz in the left channel and 660 Hz in the
right channel. The MP3 fixture is downmixed to mono. `tone-long.ogg` contains
10 seconds at 44,100 Hz to test seeks across Ogg pages and decoder overlap state.

Regenerate with FFmpeg (libmp3lame and libvorbis enabled):

```sh
FFMPEG=ffmpeg bash crates/cadence-core/tests/fixtures/generate.sh
```

The checked-in files were generated with FFmpeg 8.1.2. Tests use the committed
fixtures directly and do not need FFmpeg. The generation script fixes codec
settings, enables bit-exact output, and writes explicit title/artist/album tags.
Binary output can differ between encoder versions.

`tone-order.flac` additionally has track 7, disc 2, and Album Artist tags for
legacy catalog refresh and album ordering regressions.

The tests verify bounded reads, finite audible samples, stereo channel separation
or mono duplication, EOF, precise seeks before and after the 250 ms pre-roll
window, alignment against uninterrupted decoding, and restart after EOF. Long
Vorbis seeks at 1,507, 5,500, and 9,500 ms compare remaining frame counts and samples.
A truncated FLAC regression ensures genuine truncation remains an error.

Title, artist, and album are checked for MP3, FLAC, Vorbis, AAC in M4A, ALAC in M4A,
and AIFF with ID3. Raw ADTS AAC has no tags in this fixture. Symphonia 0.6.1 does not
expose CAF `info` tags, so CAF uses the filename fallback. The same dependency also
drops WAV LIST INFO tags; the separate generated WAV/ID3 test covers metadata read
by the probe.
