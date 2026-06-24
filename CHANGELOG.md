# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `rust-toolchain.toml` pinning the toolchain for reproducible builds.
- `Dockerfile` for a containerized server image.
- `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1) and this changelog.
- Dependabot config and a cached, lint-all-targets CI pipeline.

### Changed
- Snapshot loading now returns errors on truncated/corrupt files instead of
  panicking.
- Bumped `rand` to 0.9.

## [0.1.0] - 2026-06-24

Initial open-source release.

### Added
- In-memory RDF triple store with three flat-CSR permutation indexes (SPO/POS/OSP)
  and a layered delta overlay for incremental insert/delete.
- Dual-mode dictionary: zero-copy memory-mapped base + interned overlay.
- Hybrid query engine: worst-case-optimal join (leapfrog triejoin) for cyclic
  patterns, cost-based pipelined plan for acyclic patterns.
- SPARQL over HTTP: SELECT/ASK, BGP, OPTIONAL, UNION, FILTER (3-valued logic),
  ORDER BY, property paths, `INSERT DATA`/`DELETE DATA` with a write-ahead log.
- Zero-copy mmap snapshots and a streaming N-Triples loader.
- WDBench benchmark harness and a GraphRAG example.

[Unreleased]: https://github.com/cpthappy/trillian/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/cpthappy/trillian/releases/tag/v0.1.0
