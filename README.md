# Hearthsound slskd acquisition plugin

A [Hearthsound](https://github.com/hearthsound) acquisition plugin that finds and
downloads files from the Soulseek network through a
[slskd](https://github.com/slskd/slskd) instance. It compiles to a WebAssembly
component implementing the `acquisition` provider interface at track and album
granularity: the server hands the plugin a wanted track, or a wanted album with
its tracks, the plugin searches slskd for candidate files, enqueues the selected
candidate through the slskd batch download API, and reports the files it
produced once they are on disk in the instance's import root.

A track query answers with one candidate per audio file. An album query answers
with one candidate per peer folder, holding every audio file of that folder and
no track indices: an album lives in one directory, so the server maps the
folder's files to the album's tracks and the whole album downloads from one peer
in one batch.

The plugin only talks to slskd: slskd owns the Soulseek connection, the peer
download queue and the download directory, and the plugin reads its HTTP API.

## Requirements

- Rust 1.99 or newer, with the `wasm32-wasip2` target. The included
  `rust-toolchain.toml` pins both; `rustup toolchain install` fetches them.
- slskd 0.26.0 or newer, reachable over HTTP from the server that hosts the
  plugin, with an API key. The batch download API this plugin uses arrived in
  0.26.0, so `validate-config` refuses an older instance.
- An import root whose path is slskd's downloads directory. The plugin names
  that root by id and never sees its path.

## Build

```console
cargo build --release --locked --target wasm32-wasip2
```

The component lands at
`target/wasm32-wasip2/release/hearthsound_plugin_slskd.wasm`.

## Check and pack

Install the plugin CLI from the SDK repository:

```console
cargo install --locked --git https://github.com/hearthsound/plugin-sdk --tag v0.3.0 hearthsound-plugin-cli
```

Then validate the manifest and the component:

```console
hsplugin check
```

It prints `ok: app.hearthsound.slskd 0.1.1 (provides acquisition)` on success.

Finally, build the archive you install on a server:

```console
hsplugin pack --out-dir dist
```

`dist/app.hearthsound.slskd-0.1.1.hsplugin` contains the manifest and the
component.

## Tests

The protocol mapping, the search-text rules, the token codecs and the transfer
state mapping run on the host target, so their unit tests need no WebAssembly
toolchain and no slskd instance:

```console
cargo test
```

## Configuration

| Instance setting | Default | Meaning |
|---|---|---|
| `url` | `http://slskd:5030` | Base URL of the slskd instance. |
| `apiKey` | - | slskd API key, sent as `X-API-Key`. A `readwrite` key or better. |
| `importRoot` | - | Id of the import root whose path is slskd's downloads directory. |
| `searchTimeoutMs` | `15000` | How long one slskd search may run, in milliseconds. |
| `responseLimit` | `100` | Maximum number of peer responses kept per search. |
| `fileLimit` | `10000` | Maximum number of files kept per search. |
| `minUploadSpeedKbps` | `0` | Peers slower than this are dropped by slskd. |
| `maxPeerQueueLength` | `50` | Peers with a longer upload queue are skipped. |
| `preferredExtensions` | `["flac", "mp3"]` | Audio extensions treated as candidates. |

The instance requests no host-side rate limit: a search costs a handful of
requests, and the server already bounds how many searches run at once.

## Notes

- `searchTimeoutMs` is sent to slskd as its `searchTimeout` field, which slskd
  documents as seconds but passes to Soulseek.NET as milliseconds. The
  configuration therefore names milliseconds, and the plugin sends the value
  unchanged.
- slskd's documented defaults for `fileLimit`, `responseLimit`,
  `maximumPeerQueueLength` and `minimumPeerUploadSpeed` are not the defaults its
  search actually uses, so the plugin always sends all four explicitly.
  `minUploadSpeedKbps` is converted to the bytes per second the API expects
  (1 kbps is 125 bytes per second).
- Search responses are only readable once a search is complete; until then
  slskd holds them in memory. The plugin polls `isComplete` and then reads the
  responses, and deletes each search it has read so slskd keeps no search rows
  for it.
- A query widens in steps. It first asks for `<artist> <title>`, then for the
  same text with bracketed parts of the title removed, and only when neither
  produced a candidate and the title has more than one word does it ask for the
  bare title and keep the responses whose path names the artist. Soulseek only
  matches peers whose file path contains every query term, which is why the
  plugin keeps punctuation, collapses only whitespace, and widens in that order.
- An album query widens the same way around the album: `<album-artist> <album>`,
  then that text with the bracketed parts of the album removed, then the bare
  album keeping only the files whose path names the album artist. A step runs
  only when the step before it produced no candidate.
- An album candidate is a peer folder: its title is the folder's name, its path
  is the remote directory and its files are every audio file of that folder,
  without a `track-index`. The server decides which file is which track, and
  because all files of a folder share one directory they are enqueued in one
  batch.
- Each candidate carries a token holding the peer name and the exact remote
  files, so a candidate survives the server recycling a plugin instance.
- `start-download` enqueues one batch per remote directory with
  `options.destination = "hearthsound/<key>"`, where `<key>` is a random,
  filesystem-safe key. slskd flattens a batch's files to their basenames under
  that destination, and its `externalId` option is accepted but never stored, so
  the destination path is what correlates a download with its files.
- `poll-download` reports `complete` only when slskd reports every enqueued file
  as `Completed, Succeeded` and each file is present at its pinned path with the
  expected size. slskd can report a transfer as complete while the file is still
  in its `incomplete` directory. If a batch is gone from slskd, the plugin
  completes the download when every file is present at its pinned path, and
  otherwise reports not-found so the server restarts the search.
- Finished transfers are cleared from slskd with a record-only delete
  (`?remove=true`), which leaves the files on disk untouched.
- slskd's own error statuses map to provider errors: `401` and `403` to
  unauthorized, `429` to rate-limited (or unavailable on the batch route, which
  throttles instead of retrying), a `404` that means an unknown search or batch
  to not-found, and `5xx`, network and timeout failures to unavailable.

## Layout

| Path | Contents |
|---|---|
| `Cargo.toml` | Crate metadata and the SDK dependency. |
| `src/lib.rs` | The plugin: configuration, lifecycle and the acquisition interface. |
| `src/api.rs` | The slskd HTTP transport and its error mapping. |
| `src/config.rs` | The instance configuration and its schema-level checks. |
| `src/download.rs` | Batch grouping, destinations and pinned paths. |
| `src/search.rs` | Search-text widening and search responses to track and album candidates. |
| `src/slskd.rs` | Serde types for the slskd API. |
| `src/states.rs` | slskd transfer states to download states. |
| `src/tokens.rs` | The opaque token codecs. |
| `src/url.rs` | URL validation, joining and encoding. |
| `src/version.rs` | slskd version parsing and the 0.26.0 requirement. |
| `manifest.json` | The plugin manifest the server reads. |

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your
option.
