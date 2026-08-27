# Roadmap

Things we'd like to add or improve. Contributions welcome — for larger items,
open an issue first to agree on the approach (see [CONTRIBUTING.md](CONTRIBUTING.md)).

## SPARQL features

- Sub-`SELECT` and nested/multiple `OPTIONAL` patterns.
- `BIND` combined with `?infer=rdfs` (currently rejected as "unsupported WHERE
  pattern" — the RDFS rewrite path doesn't route through the write-locked
  `eval_where_mut` that `BIND` needs).
- Property-path edge cases: tighten result-count parity on the remaining
  WDBench paths/C2RPQ deviations (notably blank-node-bearing transitive paths).
- Pipeline execution across `OPTIONAL`/`LeftJoin` so those classes get the same
  early-termination/row-cap benefits as plain BGPs (currently the largest source
  of `Error` results on the `opts` benchmark class).

## Storage & performance

- Stop allocating per row when a term becomes a value: `decode_type`
  (`src/hypertrie/dictionary.rs`) allocates the datatype IRI for every
  `D`-prefixed key, under every `FILTER`, `ORDER BY`, `BIND` and aggregate.
  It needs a borrowed/`Cow` term type.
- Derive `pred_subjects` on demand from the index (the last predicate-keyed list
  still held in owned RAM), or back it by a `BTreeSet` for O(log n) deletes.
- WAL checkpointing / snapshot rotation.
- A structure-sharing index (sharing common subtrees across the three
  permutations) to cut the dominant index footprint — a larger undertaking.

## Data model & I/O

- Full RFC 3986 reference resolution in the Turtle parser. `@base` plus a
  protocol-relative `<//host/path>` currently keeps the base authority
  (`http://example.org//host/path`) instead of replacing it (`http://host/path`).
  The absolute, empty, fragment, absolute-path and relative-path forms are
  handled; only the `//authority` one is not.

## Tooling

- A fixed performance suite (large-result queries) as a regression guard in CI.
