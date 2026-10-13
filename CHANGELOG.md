# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- The repository scaffolding: the toolchain pin, the dual MIT/Apache-2.0 license
  files and this changelog.
- Configuration validation and the health probe: the manifest's configuration
  defaults, the schema-level checks, and a live slskd version check that refuses
  an instance older than 0.26.0.
