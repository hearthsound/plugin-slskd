# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-10-25

### Added

- Failed downloads leave nothing behind: a batch that slskd partly refuses is
  cancelled before the error, and a later batch that fails cancels the batches
  already enqueued, so no file keeps downloading without a token the server can
  cancel.
- A completed download is accepted at either path slskd can write it to, the
  batch's flattened name or the file's remote subfolder, and the plugin reports
  the path it found.

### Changed

- A download with at least one file moving is reported as `running` even when
  other files of the candidate still wait in a peer's queue, so the server
  applies the stall timeout to a download that is making progress instead of the
  queue timeout.
- A download whose batches slskd no longer knows and whose files are not at
  their paths is cancelled and reported as `failed`, instead of not-found: the
  failure no longer leaves the download's sibling batches running.
- Finished transfers are cleared from slskd with `?remove=false`, so a file the
  host has not imported yet cannot be deleted; a cancelled download is cleared
  with `?remove=true`, which deletes only the files under the plugin's own
  destination directory.
- `cancel-download` logs why it cannot reach the instance instead of returning
  silently.
- Build against the `hearthsound:plugin@0.4.0` API (plugin-sdk v0.4.0).

## [0.2.0] - 2026-10-23

### Added

- Album search and acquisition: the plugin advertises the `album` granularity,
  searches for `<album-artist> <album>` and its fallbacks, and answers with one
  candidate per peer folder holding every audio file of that folder, so a whole
  album downloads from one peer directory in one batch.

### Changed

- The plugin targets plugin API `hearthsound:plugin@0.3.0` and builds against
  SDK v0.3.0. No behaviour changes.

## [0.1.1] - 2026-10-16

### Changed

- The plugin targets plugin API `hearthsound:plugin@0.2.0` and builds against
  SDK v0.2.0. No behaviour changes.

## [0.1.0] - 2026-10-13

### Added

- The repository scaffolding: the toolchain pin, the dual MIT/Apache-2.0 license
  files and this changelog.
- Configuration validation and the health probe: the manifest's configuration
  defaults, the schema-level checks, and a live slskd version check that refuses
  an instance older than 0.26.0.
- Track search over slskd: the widening query plan, the mapping from search
  responses to candidates with its queue, extension and lock filters, the
  candidate tokens, and the store entry that carries a search between polls,
  plus the manifest that declares the plugin.
- Candidate download through the slskd batch API: the download key, the batch
  per remote directory with a pinned destination, the download token, and the
  record-only cleanup of a cancelled transfer.
- Download monitoring: the transfer state mapping, the queue and progress
  reports, the pinned-path check that keeps a download running until every file
  is on disk, the not-found that tells the server to restart a lost download,
  and the record-only cleanup of finished transfers.
- A CI workflow that formats, lints, tests, builds, checks and packs the
  plugin, and uploads the archive.

[Unreleased]: https://github.com/hearthsound/plugin-slskd/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/hearthsound/plugin-slskd/releases/tag/v0.3.0
[0.2.0]: https://github.com/hearthsound/plugin-slskd/releases/tag/v0.2.0
[0.1.1]: https://github.com/hearthsound/plugin-slskd/releases/tag/v0.1.1
[0.1.0]: https://github.com/hearthsound/plugin-slskd/releases/tag/v0.1.0
