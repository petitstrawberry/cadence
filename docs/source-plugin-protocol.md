# External source plugins (protocol v1)

Cadence can run an explicitly configured executable as a music source. This is
separate from CLAP audio effects: source plugins browse remote catalogs and resolve
media/artwork; CLAP processors work on decoded PCM audio. The same source protocol
can be implemented in Rust, Python, or another language without linking Cadence.

Configure a source instance with `plugin_id: "process"`:

```json
{
  "id": "my-service",
  "name": "My music service",
  "plugin_id": "process",
  "settings": {
    "executable": "/absolute/path/to/my-source-plugin",
    "args": [],
    "config": { "endpoint": "http://music-server:8080" }
  }
}
```

`executable` must be an absolute path. `args` defaults to an empty array and
`config` defaults to JSON null. Only `config` is forwarded as provider settings;
the executable path and arguments are host configuration. The source instance
`id` must remain stable: it namespaces every catalog item and artwork reference.

External plugins execute with the user's permissions. Cadence does not sandbox
them or automatically discover/download executables. Choose trusted local code.
Provider secrets belong in source configuration or the plugin's own credential
store, never in browse results. Configuration files containing secrets require
appropriate filesystem permissions.

## Lifecycle and framing

For each operation Cadence starts a fresh process, writes exactly one UTF-8 JSON
line to stdin, and reads exactly one UTF-8 JSON line from stdout. Every response
must end in `\n` and be flushed. Do not write logs to stdout. Stderr is discarded
to prevent a full logging pipe from deadlocking a provider.

The host keeps stdin open and retains the child during media streaming (including pause), explicit download, or artwork retrieval.
A provider that owns a temporary streaming session can maintain it until stdin
EOF, then release it. A stateless resolver may exit successfully after flushing
its response. A nonzero exit during a transfer fails the operation when the host
next checks the child. On completion/error/cancellation the host closes stdin,
kills the child if necessary, and reaps it. Cleanup must also tolerate abrupt
termination; remote leases should expire if the client disappears.

Do not spawn descendants that inherit stdin/stdout. Only the direct child is
managed. The response read/write worker is deliberately not joined on timeout,
because an inherited pipe could remain open after the direct child exits.
At most eight pipe workers may exist at once; a broken descendant that keeps a
pipe open consumes one of these slots until it closes. This bounds thread growth
without hanging the application. Trusted plugins remain responsible for their
own descendants and external resources.

Requests are limited to 128 KiB, responses to 4 MiB, and a response must arrive
within ten seconds. A browse page may contain at most 10,000 total entries.
Individual metadata strings and URLs are limited to 32 KiB. Transfer byte/time
limits and cancellation are enforced by Cadence's network transport. The process
response timeout also covers a child that refuses to read its request.

The protocol uses `api_version: 1` and a matching integer `id`. There is one
request per process, currently using `id: 1`. A response must contain exactly one
of `result` (an object) or `error` (a string); omit the other field.

```json
{"api_version":1,"id":1,"error":"Authentication required"}
```

Unknown response envelope/result fields, unsupported versions, mismatched IDs,
incomplete JSON lines, and invalid metadata are rejected. Do not include media
URLs in catalog metadata: resolve them when an item is played.

## Browse

```json
{"api_version":1,"id":1,"method":"browse","params":{"source_id":"my-service","settings":{"endpoint":"http://music-server:8080"},"container":null}}
```

`container` is null for the root, otherwise an opaque container ID returned by a
previous browse operation. For large catalogs expose containers such as artist,
album, or page IDs. The host does not assume IDs are paths or URLs.

```json
{"api_version":1,"id":1,"result":{"containers":[{"id":"album:2","title":"An album"}],"tracks":[{"item_id":"track:7","title":"A song","artist":"An artist","album":"An album","album_id":"album:2","album_artist":"An artist","track_number":7,"disc_number":1,"duration_seconds":180.5,"artwork_item_id":"cover:2"}]}}
```

`album_id`, `album_artist`, `track_number` and `disc_number` are optional v1 fields.
Use a stable `album_id` to group tracks in the album view; the host namespaces it
by source ID. Track and disc numbers are unsigned integers. Names are retained
as supplied, without splitting artist names or inferring compilations.

`containers` and `tracks` each default to an empty array. A track requires a
nonempty `item_id` and a `title`; the other fields may be omitted or null.
Duration must be nonnegative and fit Rust's `Duration`. IDs within each list must be unique.
Cadence computes `track_id(source_id, item_id)` and stores
`MediaLocation::Provider`, plus an optional artwork provider identity. The plugin
cannot override the source instance, stable track ID, or local media path.

## Resolve media

```json
{"api_version":1,"id":1,"method":"resolve","params":{"source_id":"my-service","settings":{},"item_id":"track:7"}}
{"api_version":1,"id":1,"result":{"url":"http://music-server:8080/temporary-stream/abc"}}
```

Playback opens the result as a progressive, bounded-memory HTTP stream on the
playback worker; no complete-file download or media cache is required. Range-capable
servers support seeking. Servers returning a full HTTP 200 response are treated as
forward-only sources and still support pause/resume. The child lease stays alive
until the stream is closed. Every new playback open resolves a fresh URL. URLs may use HTTP or HTTPS and must have a host and no embedded
username/password. Authentication may use short-lived signed URLs; custom request
headers are not part of v1. Do not depend on Cadence preserving a temporary URL.
Plugins may expose a loopback HTTP server while the child is retained if the
upstream service needs a different protocol or authenticated request headers.

## Resolve artwork

```json
{"api_version":1,"id":1,"method":"artwork","params":{"source_id":"my-service","settings":{},"item_id":"cover:2"}}
{"api_version":1,"id":1,"result":{"url":"http://music-server:8080/cover/2.png"}}
```

The same URL rules and child lifecycle apply. The host bounds downloaded artwork
and decoded image dimensions before it reaches the GUI.

## Example and platform support

[`examples/source-plugin/catalog.py`](../examples/source-plugin/catalog.py) is a
small Python 3 provider reading a local JSON manifest. Configure the absolute
Python interpreter as `executable`, the absolute script path as its only argument,
and `config.manifest` as the absolute manifest path. Its companion
[`catalog.json`](../examples/source-plugin/catalog.json) demonstrates the manifest
format. Supply accessible media/artwork URLs before playing its demo entry.

The host uses portable Rust process APIs and does not require Python. The example
does require an installed Python interpreter. Desktop HTTPS uses the host
transport's verified TLS implementation.

External executable sources are currently disabled on Scarlet: its Rust standard
library supports spawning/waiting but `Child::kill` is not implemented. Cadence
rejects a launch rather than create a child that cancellation cannot stop. The
same v1 protocol can be enabled once Native Kill and its standard-library binding
are implemented and tested. The built-in Kanade and DLNA providers remain
available on Scarlet; they use HTTP because Scarlet TLS is not yet supported.
Native Scarlet executables must also match its executable ABI. Compilation checks
do not establish that a particular plugin/runtime or service works on hardware.
