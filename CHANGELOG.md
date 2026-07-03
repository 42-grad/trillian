# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-07-03

### Added
- **RDFS backward-chaining inference** — query-time rule engine via SPARQL
  algebra rewriting (`src/inference.rs`). Supported rules: `rdfs:subClassOf`,
  `rdfs:subPropertyOf`, `rdfs:domain`, `rdfs:range`. Enable with
  `?infer=rdfs` on `/sparql`, `/stream`, and `/count` endpoints. No index
  changes — inference is purely at query time.

## [0.1.4] - 2026-06-25

### Added
- Project website at [trilliandb.org](https://trilliandb.org) built with Astro,
  deployed via GitHub Pages — quickstart, feature overview, GraphRAG tutorial,
  and imprint.
- Custom domain `trilliandb.org` for the project site.
- `cargo deny check` as a CI supply-chain gate (`deny.toml` policy).
- `AGENTS.md` with branching, release, and CI conventions for AI (and human)
  contributors.
- GraphRAG demo now prints latency (per-phase and total), timestamp, and
  process RSS on every run.

### Fixed
- GraphRAG demo: `from mistralai import Mistral` → `from mistralai.client import
  Mistral` for compatibility with `mistralai >= 2.5.0`.

### Changed
- GitHub URLs throughout updated from the archived `anomalyco` org to `42-grad`.

## [0.1.3] - 2026-06-24

### Changed
- Repository moved to the **42-grad** organization; references updated.
- Dependency updates: `rand` 0.9 → 0.10 (`random_range` moved to the `RngExt`
  trait), `lru` 0.13 → 0.18, `string-interner` 0.17 → 0.20, `memmap2` 0.9.11;
  CI action `download-artifact` v4 → v8.

## [0.1.2] - 2026-06-24

### Fixed
- `sameTerm` now does exact RDF-term equality (kind + lexical + datatype + lang)
  instead of value equality, so `"1"^^xsd:integer` and `"1"^^xsd:double` are no
  longer treated as the same term.
- `/stream` no longer falls back to column 0 when a SELECT variable is absent;
  the column is omitted instead of emitting a wrong value.

### Security
- `MappedDict::key` uses checked UTF-8 conversion (no more `from_utf8_unchecked`),
  and snapshot loading validates the key blob is UTF-8 and uses checked
  arithmetic for the dictionary bounds check — a corrupt/hostile snapshot can no
  longer trigger undefined behaviour or bypass the bounds check.
- HTTP request bodies are capped (`TRILLIAN_MAX_BODY_BYTES`, default 64 MiB) to
  prevent OOM from oversized POSTs.

### Changed
- `FILTER` `||`/`&&` now short-circuit (3-valued semantics unchanged).

### Removed
- Dead `intersect_bitmap`, dropping the `roaring` dependency.

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

[Unreleased]: https://github.com/42-grad/trillian/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/42-grad/trillian/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/42-grad/trillian/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/42-grad/trillian/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/42-grad/trillian/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/42-grad/trillian/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/42-grad/trillian/releases/tag/v0.1.0
