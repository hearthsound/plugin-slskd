# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/hearthsound/plugin-slskd/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/hearthsound/plugin-slskd/releases/tag/v0.1.1
[0.1.0]: https://github.com/hearthsound/plugin-slskd/releases/tag/v0.1.0
