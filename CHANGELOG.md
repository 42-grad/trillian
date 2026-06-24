# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-06-24

### Changed
- Reworked the GraphRAG example into a real ingestion pipeline:
  `ingest_wikipedia.py` builds a knowledge graph from a Wikipedia article via LLM
  triple extraction, with a bundled "Hitchhiker's Guide to the Galaxy" graph so
  the demo runs offline. Fixed literal-object handling in the retrieval step.

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
- Public `sparql::execute_sparql` so embedders can run queries without the HTTP
  server.
- End-to-end integration tests under `tests/` (SPARQL over the public API,
  snapshot round-trip).
- Project hygiene for open source: `rust-toolchain.toml`, `Dockerfile`,
  `CODE_OF_CONDUCT.md`, this changelog, Dependabot, a cached CI pipeline
  (fmt + clippy `--all-targets` + tests), and a `.githooks/pre-commit` hook.
- Tag-triggered release workflow publishing downloadable binaries (Linux +
  macOS), plus a protected `master`/`develop` branching model.

### Changed
- Snapshot loading returns errors on truncated/corrupt files instead of
  panicking (bounds-checked readers).
- Bumped `rand` to 0.9.
- Refactored `eval_path` into focused helpers.

### Removed
- The test-only `Stats` cardinality helper is gated out of release builds.

[Unreleased]: https://github.com/cpthappy/trillian/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/cpthappy/trillian/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/cpthappy/trillian/releases/tag/v0.1.0
