# Roadmap

Things we'd like to add or improve. Contributions welcome — for larger items,
open an issue first to agree on the approach (see [CONTRIBUTING.md](CONTRIBUTING.md)).

## SPARQL features

- Sub-`SELECT` and nested/multiple `OPTIONAL` patterns.
- `BIND` combined with `?infer=rdfs` (currently rejected as "unsupported WHERE
  pattern" — the RDFS rewrite path doesn't route through the write-locked
  `eval_where_mut` that `BIND` needs).
- `GROUP BY` with `COUNT(*)` is done; Same goes for `MIN`, `MAX`, and `SAMPLE`.
  The remaining aggregates (`SUM`, `AVG`, `GROUP_CONCAT`, and `COUNT(?x)`,
  which counts bound values rather than solutions, and so is a separate case
  from `COUNT(*)`) are planned for follow-up. Aggregate arguments must
  currently be a bare variable: `MIN(?v)` works, `MIN(?v + 1)` is rejected.
- Property-path edge cases: tighten result-count parity on the remaining
  WDBench paths/C2RPQ deviations (notably blank-node-bearing transitive paths).
- Pipeline execution across `OPTIONAL`/`LeftJoin` so those classes get the same
  early-termination/row-cap benefits as plain BGPs (currently the largest source
  of `Error` results on the `opts` benchmark class).

## Storage & performance

- Derive `pred_subjects` on demand from the index (the last predicate-keyed list
  still held in owned RAM), or back it by a `BTreeSet` for O(log n) deletes.
- WAL checkpointing / snapshot rotation.
- A structure-sharing index (sharing common subtrees across the three
  permutations) to cut the dominant index footprint — a larger undertaking.

## Data model & I/O

- Turtle (`.ttl`) input in addition to N-Triples.
- Quoted/escaped literal coverage beyond the current benchmark needs.

## Tooling

- A fixed performance suite (large-result queries) as a regression guard in CI.
