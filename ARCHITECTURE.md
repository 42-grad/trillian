# Architecture

Trillian is an in-memory RDF triple store and SPARQL engine. It is built around
a few deliberate choices: flat, cache-friendly indexes; `u32` term IDs instead
of pointers; zero-copy memory-mapped persistence; and a hybrid query engine that
picks worst-case-optimal joins for cyclic patterns and a cost-based plan for the
rest.

```
N-Triples ─▶ Dictionary (string ⇄ u32) ─▶ three CSR permutation indexes
                                              SPO · POS · OSP
                                                   │
SPARQL/HTTP ─▶ parser ─▶ engine ─▶ {cost-based planner | WCOJ leapfrog} ─▶ rows ─▶ JSON
```

## Term dictionary (`hypertrie/dictionary.rs`)

Every IRI, literal, and blank node is interned to a `u32` ID. The type of a term
(IRI / typed literal / language literal / blank node) is encoded in the interned
key's prefix, so there is no separate per-term type table.

Two space optimizations matter at scale:

- **Namespace folding** — long, repeated IRI prefixes (e.g. Wikidata's
  `entity/`, `prop/direct/`, XSD/RDF namespaces) are folded to a 2-byte escape
  inside the key and expanded on read.
- **Dual-mode storage** — while building/updating, terms live in an owned
  interner. Loaded from a snapshot, the dictionary is **memory-mapped**: keys,
  offsets, and a sorted-id index live in the file (zero owned RAM), key→id is a
  binary search over the mmap, and id→key is a zero-copy slice. Terms added
  after load go into a small owned overlay (ids ≥ the mapped base) — the same
  base+delta idea as the index.

## Indexes (`hypertrie/index.rs`)

The store keeps **three permutations** of every triple — SPO, POS, OSP — so any
access pattern with at least one bound term can be answered by a sorted slice.

Each permutation is a **flat CSR arena**: a three-level structure
(`keys`, `key_off`, `l1`, `l1_off`, `vals`) held in a few large contiguous `u32`
vectors rather than per-node allocations. This keeps the data cache-friendly and
makes it trivially memory-mappable.

- **`U32Arena`** backs each of the five arrays as either `Owned(Vec<u32>)` or
  `Mapped` (a zero-copy slice into an `Arc<Mmap>`).
- **Base + delta** — the immutable CSR base carries a small delta overlay
  (`ins`/`del`) so updates stay incremental; reads return `Cow<[u32]>` (borrowed
  from the base when the delta doesn't touch a leaf, merged otherwise). The
  delta compacts back into a fresh base when it grows.

Cardinality estimates for the planner (`sp`/`po`/`os` pair counts, term degrees)
are derived **on demand** from the indexes via the `CardEstimator` trait — there
are no stored statistics maps.

## Query engine (`hypertrie/engine.rs`, `executor.rs`, `planner.rs`)

`HybridEngine` inspects the basic graph pattern's variable graph:

- **Cyclic** patterns (e.g. triangles) → **WCOJ** (worst-case-optimal join) via
  leapfrog intersection of sorted candidate slices.
- **Acyclic** patterns (stars, paths, trees) → a cost-based, left-deep plan
  executed by a **pipelined** depth-first executor: each partial row is carried
  to completion rather than materializing every join level, so memory is bounded
  to roughly the output size plus recursion depth. A `LIMIT` is pushed down and
  terminates execution early.

Results are materialized into a **`RowBlock`** — a single row-major `Vec<u32>`
rather than `Vec<Vec<u32>>`, avoiding millions of tiny allocations. A configurable
row cap (`TRILLIAN_MAX_ROWS`) makes a degenerate/cross-product query fail with a
clean error instead of exhausting memory.

## SPARQL layer (`sparql.rs`)

Parses with `spargebra` and evaluates `SELECT`/`ASK` over BGPs plus `OPTIONAL`,
`UNION`, `FILTER` (3-valued logic), `ORDER BY`, projection/`DISTINCT`/`LIMIT`/
`OFFSET`, and property paths (`/ ^ | * + ?` and negated property sets). Results
are streamed directly to SPARQL-JSON. The HTTP server (`axum`) exposes
`/sparql`, `/stream`, `/count`, and `/update`.

## Persistence (`hypertrie/query.rs`, `wal.rs`)

- **Snapshot** — the index arrays and the dictionary are written to a single
  file with a versioned header; `load` memory-maps it, so the index and
  dictionary are served zero-copy and only paged in as touched.
- **WAL** — `INSERT DATA`/`DELETE DATA` updates are appended to a write-ahead
  log and `fsync`'d, then replayed on top of the snapshot at startup, so durable
  updates survive a restart without rewriting the whole snapshot.

## Source map

| Path | Contents |
| --- | --- |
| `hypertrie/dictionary.rs` | term ⇄ u32, type-encoded keys, namespace folding, mmap/overlay |
| `hypertrie/index.rs` | flat-CSR `LayeredIndex`, `U32Arena`, base+delta |
| `hypertrie/planner.rs` | `GraphPattern`, cost-based join ordering, `CardEstimator` use |
| `hypertrie/executor.rs` | `RowBlock`, pipelined plan executor, WCOJ leapfrog, row cap |
| `hypertrie/engine.rs` | cyclic/acyclic routing |
| `hypertrie/query.rs` | `TripleStore`: ingest, queries, updates, snapshot |
| `hypertrie/stats.rs` | `CardEstimator` trait |
| `hypertrie/export.rs` | N-Triples parser/serializer |
| `sparql.rs` | SPARQL evaluation + HTTP endpoint |
| `wal.rs` | write-ahead log |
| `bin/server.rs` | `build` / `load` / `profile` CLI |
