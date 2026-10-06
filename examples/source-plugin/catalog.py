#!/usr/bin/env python3
"""Minimal Cadence v1 source provider; no third-party Python dependencies.

Run through an explicitly configured Python interpreter. The caller passes
settings.manifest, an absolute path to a JSON array of stable catalog entries.
The manifest's media/artwork URLs are resolved on demand and never browsed into
Cadence's persistent library.
"""

import json
import pathlib
import sys

MAX_REQUEST = 128 * 1024
MAX_MANIFEST = 4 * 1024 * 1024


def load_manifest(settings):
    path = pathlib.Path(settings["manifest"])
    if not path.is_absolute():
        raise ValueError("manifest must be an absolute path")
    with path.open("rb") as handle:
        content = handle.read(MAX_MANIFEST + 1)
    if len(content) > MAX_MANIFEST:
        raise ValueError("manifest exceeds 4 MiB")
    entries = json.loads(content)
    if not isinstance(entries, list):
        raise ValueError("manifest must contain an array")
    if len({entry["item_id"] for entry in entries}) != len(entries):
        raise ValueError("manifest contains duplicate item IDs")
    return entries


def dispatch(request):
    if request.get("api_version") != 1:
        raise ValueError("unsupported protocol version")
    params = request["params"]
    entries = load_manifest(params["settings"])
    method = request["method"]
    if method == "browse":
        if params.get("container") is not None:
            raise ValueError("this example has no child containers")
        tracks = []
        for entry in entries:
            track = {
                key: entry[key]
                for key in ("item_id", "title", "artist", "album", "duration_seconds")
                if key in entry
            }
            if entry.get("artwork_url"):
                track["artwork_item_id"] = entry["item_id"]
            tracks.append(track)
        return {"containers": [], "tracks": tracks}
    if method not in ("resolve", "artwork"):
        raise ValueError("unknown method")
    entry = next((entry for entry in entries if entry["item_id"] == params["item_id"]), None)
    if entry is None:
        raise ValueError("unknown item ID")
    url = entry.get("media_url" if method == "resolve" else "artwork_url")
    if not url:
        raise ValueError("requested resource is unavailable")
    return {"url": url}


def main():
    request_id = 1
    try:
        line = sys.stdin.buffer.readline(MAX_REQUEST + 1)
        if len(line) > MAX_REQUEST or not line.endswith(b"\n"):
            raise ValueError("invalid request framing or size")
        request = json.loads(line)
        request_id = request.get("id", 1)
        response = {"api_version": 1, "id": request_id, "result": dispatch(request)}
    except (KeyError, TypeError, ValueError, OSError) as error:
        response = {"api_version": 1, "id": request_id, "error": str(error)}
    print(json.dumps(response, separators=(",", ":")), flush=True)
    # A provider with a session would maintain it until EOF here. The host may
    # terminate us promptly after closing stdin, so remote leases must expire.
    while sys.stdin.buffer.read(4096):
        pass


if __name__ == "__main__":
    main()
