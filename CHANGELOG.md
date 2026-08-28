# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Sub-`SELECT`** — a nested `SELECT` inside the `WHERE` clause
  (`src/sparql.rs`), with its own `DISTINCT`, `ORDER BY`, `LIMIT`/`OFFSET` and
  `GROUP BY`, joined against the enclosing pattern and nestable.
  Variables the sub-`SELECT` does not project are out of scope outside it, so
  the enclosing pattern may reuse those names. This closes the gap that made
  aggregates hard to use: an aggregate computed in the inner query can now be
  filtered and joined back against the graph
  (`{ SELECT ?s (COUNT(*) AS ?c) WHERE { ?s :knows ?o } GROUP BY ?s }
  FILTER(?c > 1) . ?s rdfs:label ?label`). A plain sub-`SELECT` works under
  `?infer=rdfs` too; one containing `GROUP BY` does not yet — see ROADMAP.
- **The remaining `GROUP BY` aggregates** — `COUNT(?x)`, `SUM`, `AVG`, `MIN`,
  `MAX`, `SAMPLE` and `GROUP_CONCAT` (with `SEPARATOR`), each accepting
  `DISTINCT` (`src/sparql.rs`); the "not supported" error from 0.3.0 is gone.
  `MIN`/`MAX`/`SAMPLE` return the stored term and keep its datatype,
  `GROUP_CONCAT` yields a plain string, and `SUM`/`AVG` intern a computed
  number so they yield `xsd:double`. Over an empty group `SUM`/`AVG` are `0`
  (an `xsd:integer`, as SPARQL 1.1 defines) and `GROUP_CONCAT` is `""`, while
  `MIN`/`MAX`/`SAMPLE` are unbound.
- **Expression arguments for `SUM`/`AVG`** — `SUM(?v + 1)`. The others still
  need a bare variable, which is what lets them return the source term.
- **Turtle (`.ttl`) input** (`src/hypertrie/turtle.rs`) — RDF 1.1 Turtle minus
  RDF-star: prefixes and `@base` (both the `@` and SPARQL-style forms), `a`,
  `;`/`,` lists, blank-node property lists, collections (expanded to an
  `rdf:first`/`rdf:rest` chain), all four quote forms, and comments. The
  numeric and boolean shorthands are typed by their shape, so a bare `30`
  filters numerically. Parse errors carry a line number.
- `TripleStore::ingest_turtle_file`, plus `ingest_rdf_file`, which picks the
  parser from the extension. The `server` binary now takes either format
  wherever it took `.nt`.

### Changed
- **Turning a term into a value no longer allocates twice per row** —
  `classify()` and `lit_key()` (`src/sparql.rs`) built a constant datatype IRI
  with `format!("{XSD}string")`/`boolean` on every call, just to compare it;
  they now use `dt.strip_prefix(XSD)`, as `is_numeric_dt` already did. This sits
  under every `FILTER`, `ORDER BY` and `BIND`. Measured over 200k typed integer
  literals: `FILTER(?v > 0)` 336 → 161 ns/row (2.1x), the same filter with two
  comparisons 591 → 241 ns/row (2.5x), `ORDER BY ?v` 369 → 229 ns/row (1.6x),
  and `BIND(?v + 1)` 570 → 414 ns/row (1.4x). Behaviour is unchanged — both
  forms test the same datatype IRI.

### Fixed
- **Non-ASCII literals were silently corrupted on ingest**
  (`src/hypertrie/export.rs`). `parse_quoted_string` walked `s.as_bytes()` and
  did `bytes[i] as char`, reading each UTF-8 byte as a Latin-1 codepoint, so
  `"café"` loaded as `"cafÃ©"` — no error, and every existing test was ASCII.
  Parsing now iterates characters, and the escapes the old code missed (`\'`,
  `\b`, `\f`) are supported; `escape_literal` emits `\b`/`\f` so serialization
  round-trips.
- **A sub-`SELECT`'s modifiers no longer leak to the enclosing query.**
  `peel_modifiers` walked the whole `Slice`/`Distinct`/`Project`/`OrderBy`
  chain in one loop, so on a nested modifier stack the inner `SELECT`'s
  projection overwrote the outer one and its `LIMIT` was attributed to the
  outer level. It now stops at the sub-`SELECT` boundary, detected by the fixed
  `Slice > Distinct > Project > OrderBy` nesting order of one query level.
  Present since before 0.3.0: a sub-`SELECT` forming the whole `WHERE` body
  already reached evaluation, and answered with the *inner* projection —
  `SELECT ?s WHERE { { SELECT * WHERE { ?s ?p ?o } } }` returned `?s`, `?p` and
  `?o`. (A sub-`SELECT` joined with anything else errored out instead, so only
  the whole-body form returned wrong results silently.)

## [0.3.0] - 2026-08-01

### Added
- **`BIND`** — computes an expression and binds it to a new variable
  (`src/sparql.rs`). A value not yet in the dictionary (e.g. an arithmetic or
  string result) is interned on the fly, so the query needs write access to
  the store for that one request; queries without `BIND` are unaffected and
  keep the fully concurrent read lock. Not yet supported together with
  `?infer=rdfs`.
- **`REGEX` in `FILTER`** — `REGEX(text, pattern, flags?)` with the `i`
  (case-insensitive), `s` (dot matches newline), and `m` (multiline) flags.
  Compiled patterns are cached process-wide to avoid recompiling per row.
- **`GROUP BY` with `COUNT`** — basic aggregation support. `COUNT(*)` and
  `COUNT(DISTINCT *)` are evaluated per group and interned as `xsd:integer`
  literals. `HAVING` works out of the box because it is represented as a
  `Filter` over the grouped results. Other aggregates (`SUM`, `AVG`, `MIN`,
  `MAX`, `GROUP_CONCAT`, `SAMPLE`) return a clear "not supported" error.

### Changed
- Queries containing `GROUP BY` now take a write lock (like `BIND`) because
  aggregate results must be interned into the dictionary. Non-aggregation
  queries keep the fully concurrent read lock.
- LIMIT pushdown is disabled when `GROUP BY` is present, so the full result
  set is available for aggregation, `HAVING`, and ordering.

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

[Unreleased]: https://github.com/42-grad/trillian/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/42-grad/trillian/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/42-grad/trillian/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/42-grad/trillian/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/42-grad/trillian/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/42-grad/trillian/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/42-grad/trillian/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/42-grad/trillian/releases/tag/v0.1.0
