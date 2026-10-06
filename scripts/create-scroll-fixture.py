#!/usr/bin/env python3
"""Create local, playable scroll fixtures without touching an existing catalog."""
import argparse
import json
import math
import os
from pathlib import Path
import struct
import wave
import zlib


def cover(path, index):
    edge = 256 + (index % 12) * 64
    rows = bytearray()
    for y in range(edge):
        rows.append(0)
        for x in range(edge):
            band = ((x // 32) ^ (y // 32)) & 1
            rows.extend(((index * 41 + x * 128 // edge) % 256,
                         (index * 67 + y * 128 // edge) % 256,
                         90 + band * 90))

    def chunk(kind, data):
        return (struct.pack('>I', len(data)) + kind + data
                + struct.pack('>I', zlib.crc32(kind + data)))

    path.write_bytes(b'\x89PNG\r\n\x1a\n'
                     + chunk(b'IHDR', struct.pack('>IIBBBBB', edge, edge, 8, 2, 0, 0, 0))
                     + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b''))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--data-dir', required=True, type=Path)
    parser.add_argument('--music-dir', required=True, type=Path)
    args = parser.parse_args()
    data = args.data_dir.expanduser().resolve()
    music = args.music_dir.expanduser().resolve()
    if (data / 'library.json').exists() or music.exists():
        parser.error('The catalog and fixture music directory must not already exist.')
    data.mkdir(parents=True, exist_ok=True)
    music.mkdir(parents=True)
    seed = music / 'reference.wav'
    with wave.open(str(seed), 'wb') as audio:
        audio.setparams((1, 2, 44100, 0, 'NONE', 'not compressed'))
        audio.writeframes(b''.join(struct.pack('<h', round(1000 * math.sin(i * 2 * math.pi * 440 / 44100)))
                                   for i in range(2 * 44100)))
    tracks = []
    characters = '風光空海星月花夢夜雨春夏秋冬音色虹雪森旅街灯波雲鳥朝夕響'
    for album in range(358):
        artist = f'Fixture Artist {album // 12:02}'
        name = f'スクロール検証 {album:03}'
        folder = music / artist / name
        folder.mkdir(parents=True)
        artwork = folder / 'cover.png'
        cover(artwork, album)
        count = 383 if album == 0 else 14
        split = (count + 1) // 2
        for n in range(count):
            disc, number = (1, n + 1) if n < split else (2, n - split + 1)
            path = folder / f'{disc:02}-{number:03}.wav'
            os.link(seed, path)
            title = ''.join(characters[(n * 7 + album * 3 + j) % len(characters)] for j in range(16))
            tracks.append(dict(id=f'5:local{path}', source_id='local', item_id=str(path),
                               title=f'{number:03} {title}', artist=artist, album=name,
                               album_id=None, album_artist=artist, track_number=number,
                               disc_number=disc, duration_seconds=2.0,
                               location=dict(kind='local', value=str(path)),
                               artwork=dict(kind='file', value=str(artwork))))
    catalog = dict(schema_version=1, ordering_metadata_version=1,
                   roots=[str(music)], tracks=tracks)
    (data / 'library.json').write_text(json.dumps(catalog, ensure_ascii=False, indent=2) + '\n')
    print(f'Created {len(tracks)} playable tracks, 358 covers, and a 383-track album in {data}')


if __name__ == '__main__':
    main()
