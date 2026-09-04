# Roadmap

Things we'd like to add or improve. Contributions welcome — for larger items,
open an issue first to agree on the approach (see [CONTRIBUTING.md](CONTRIBUTING.md)).

## SPARQL features

- `FILTER EXISTS`/`NOT EXISTS`, `COALESCE`, and the unimplemented functions
  (`SUBSTR`, `ABS`, `CONCAT`, `REPLACE`, the date and hash functions, …), all
  named by `unsupported_in_expr` and rejected today, in a top-level `FILTER`
  and inside an `OPTIONAL` alike. `EXISTS` needs the filter evaluator to reach
  `eval_where`; the functions are additions to `eval_func`.
- Nested/multiple `OPTIONAL` patterns.
- `BIND`, `GROUP BY`, aggregate sub-`SELECT`s and an unknown `VALUES` term
  combined with `?infer=rdfs`, all rejected today. One root cause: the handlers
  skip the write path whenever `infer=rdfs` is set, so the RDFS rewrite never
  routes through the write-locked `eval_where_mut` that interning needs. Fixing
  it fixes all four — see `execute_sparql_infer` (`src/sparql.rs`).
- Property-path edge cases: tighten result-count parity on the remaining
  WDBench paths/C2RPQ deviations (notably blank-node-bearing transitive paths).
- Pipeline execution across `OPTIONAL`/`LeftJoin` so those classes get the same
  early-termination/row-cap benefits as plain BGPs (currently the largest source
  of `Error` results on the `opts` benchmark class).

## Storage & performance

- Stop allocating per row when a term becomes a value: `decode_type`
  (`src/hypertrie/dictionary.rs`) still allocates the datatype IRI for every
  `D`-prefixed key and the language tag for every `G`-prefixed one, under every
  `FILTER`, `ORDER BY`, `BIND` and aggregate. Needs a borrowed/`Cow` term type.
  (The `classify()`/`lit_key()` half of this was fixed in #49 with
  `dt.strip_prefix(XSD)`.)
- A read-only `SELECT` writes to the WAL, without bound. Every term a query
  interns is logged by `log_interned_since` (`src/sparql.rs`) so that a replay
  reassigns the same IDs — necessary, but it lets reads mutate durable state.
  Measured on 175M DBLP triples: one `GROUP_CONCAT` with `GROUP BY ?pub
  LIMIT 20` returned 20 rows and wrote 120 MB, interning 1.85M terms — one per
  group, since `LIMIT` is applied after grouping. Those terms replay on every
  start, so snapshot load went 1.6 s → 7.7 s permanently and the WAL never
  shrinks. Cost tracks groups, not rows returned; a plain `SELECT` and a `BIND`
  write nothing. Not fixable by skipping the log, because replay depends on the
  ID assignment matching — it resolves with the overlay below.
- A query can still grow the dictionary permanently: `VALUES`, `BIND` and
  `GROUP BY` all intern terms a `SELECT` never stores. Inherent to handing a
  term back from a `u32` row; avoiding it needs a query-local overlay for IDs
  above the dictionary's range, which every ID-resolving site must consult.
  The in-memory half of the item above, and the shared fix for both.
- Drop the per-term UTF-8 validation on the read path: `MappedDict::key`
  (`src/hypertrie/dictionary.rs`) runs `str::from_utf8` *twice* for every bound
  term of every result row — `append_term` and `term_to_json` resolve the value
  and the type separately, and each goes through `raw_key` — re-proving what
  `from_mapped` already checks once over the whole keys blob. Validating offset
  monotonicity and char boundaries at load would let the hot path go back to
  unchecked. Measured on 100k-row results against a binary built from `b37bc56`
  (the WDBench run; the check ships in every release from `v0.1.2` on, so an
  older tag is not a usable baseline): ~6% of query time, of which the ablation
  attributes 3-6 percentage points to this check.
- Parse each query once: `query_needs_write` (`src/sparql.rs`) runs a full
  spargebra parse just to pick the read or write lock, so every uncached,
  non-`infer=rdfs` query is parsed twice (a `/sparql` cache hit returns before
  either parse; `?infer=rdfs` short-circuits the check). Parse once, inspect the
  algebra, then dispatch. Single-digit µs per request, but on every `/sparql`,
  `/stream` and `/count`.
- Derive `pred_subjects` on demand from the index (the last predicate-keyed list
  still held in owned RAM), or back it by a `BTreeSet` for O(log n) deletes.
- WAL checkpointing / snapshot rotation.
- A structure-sharing index (sharing common subtrees across the three
  permutations) to cut the dominant index footprint — a larger undertaking.

## Data model & I/O

- Quoted/escaped literal coverage beyond the current benchmark needs.

## Tooling

- A fixed performance suite (large-result queries) as a regression guard in CI.
