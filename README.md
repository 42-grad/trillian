# Tentris Clone in Rust

A high-performance RDF triple store and SPARQL endpoint written in Rust, inspired by the DICE-group research prototype [Tentris](https://github.com/dice-group/tentris).

## Goal

Build a lean, cache-friendly graph database that can run realistic SPARQL-over-HTTP benchmarks against the original C++ Tentris on identical data and queries.

## Architecture

- **Dictionary**: String ↔ `u32` term mapping with term-type metadata (IRI, literal with datatype/language, blank node).
- **LayeredIndex**: three-level index per permutation (SPO/POS/OSP) as a **flat CSR arena** (few large contiguous vectors — no per-leaf allocation) plus a small **delta overlay** (`ins`/`del`) so updates stay incremental. Reads return `Cow<[u32]>`: borrowed from the base when the delta does not touch a leaf, merged otherwise. The delta compacts into a fresh base when it grows.
- **WCOJ without redundant relations**: candidate slices for bound-predicate patterns come straight from the SPO/POS permutations; only slim sorted distinct subject/object lists per predicate are kept on the side.
- **Hybrid Query Engine**:
  - Acyclic BGPs: cost-based left-deep planner with dictionary statistics.
  - Cyclic BGPs: WCOJ (Worst-Case Optimal Join) via leapfrog intersection.
- **Streaming ingest**: N-Triples are parsed line by line and mapped into the dictionary on the fly — the full parse buffer is never materialized.
- **SPARQL Endpoint**: axum-based HTTP server exposing `/sparql`, `/stream`, `/count`, `/update`, with an LRU query cache and a reader-writer lock.
- **No pointer chasing**: flat vectors, `u32` IDs, FxHashMap, Rayon parallelism.

## Supported SPARQL Features

- `SELECT` and `ASK`
- Projection, `DISTINCT`, `LIMIT`, `OFFSET`
- Basic graph patterns (BGP)
- `OPTIONAL` (single-level left joins)
- `INSERT DATA` and `DELETE DATA` updates
- IRI, literal (`"x"`, `"x"@en`, `"x"^^<dt>`), and typed-literal constants

Not yet supported: `UNION`, `FILTER`, `BIND`, `ORDER BY`, `GROUP BY`, nested graph patterns.

## Build

```bash
cargo build --release --bin server
cargo test
```

## Run the SPARQL server

```bash
# Uses synthetic_1m.nt on port 9080 by default
./target/release/server

# Custom file and port
./target/release/server my_data.nt 9080
```

## Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/sparql` | GET/POST | Standard SPARQL 1.1 JSON results |
| `/stream` | GET/POST | NDJSON stream: header line + one binding per line |
| `/count`  | GET/POST | Result count for a SELECT/ASK query |
| `/update` | POST    | SPARQL Update (`INSERT DATA`, `DELETE DATA`) |

## Benchmark Duel vs. C++ Tentris

The repository includes a realistic remote duel (`run_remote_duel.sh`) that provisions a Hetzner Cloud server, builds both systems from source, and benchmarks them through their real `/sparql` HTTP endpoints on 1 million synthetic N-Triples.

### Latest Result (Forschungs-Tentris, 1M graph-shaped triples)

Query latency is warm median; all queries return identical row counts to Tentris.
Full history and raw logs: [`BENCHMARKS.md`](BENCHMARKS.md) and `bench/`.

| Benchmark | Rust | C++ Tentris | Winner |
| :--- | ---: | ---: | :--- |
| Ingest + startup | 804 ms | 6139 ms | Rust 7.6× |
| chain join | 7.1 ms | 15.1 ms | Rust 2.1× |
| triangle (WCOJ) | 4.3 ms | 11.2 ms | Rust 2.6× |
| star | 1.5 ms | 3.3 ms | Rust 2.2× |
| distinct | 8.5 ms | 10.5 ms | Rust 1.2× |
| optional | 36.5 ms | 27.5 ms | Tentris 1.3× |
| INSERT (triples/s) | 9.4M | 0.80M | Rust 11.8× |
| DELETE (triples/s) | 15.2M | 0.75M | Rust 20.3× |
| **Peak RSS** | **338 MB** | 268 MB | Tentris 1.26× |
| index footprint | 338 MB (RAM) | 513 MB (disk store) | Rust smaller |

The dataset is graph-shaped (shared subject/object vocabulary with planted
triangles and chains), so every join returns real, non-empty, matching results.
Memory started at 1104 B/triple and is now 354 B/triple vs Tentris' 281 — the gap
went from 3.9× to 1.26× over four optimization stages (see `BENCHMARKS.md`).

> Caveat: this is 1M RAM-friendly synthetic data. Tentris targets large,
> persistent, real-world RDF where its disk-backed store and compression matter.

## Running the Remote Duel

Requirements locally:
- `terraform`
- `ansible`
- `HCLOUD_TOKEN` environment variable

```bash
export HCLOUD_TOKEN=...
./run_remote_duel.sh
```

The script creates a dedicated SSH key at `infra/terraform/duel_key`, provisions the server, runs the duel, and fetches `duel_output.log`. Remember to tear down the infrastructure when finished:

```bash
./infra/terraform/destroy.sh
```

## Project Layout

```
src/
  hypertrie/
    dictionary.rs   # Term dictionary with types
    index.rs        # Flat-CSR LayeredIndex + delta overlay
    stats.rs        # Cardinality statistics
    planner.rs      # BGP optimizer and GraphPattern
    executor.rs     # Plan executor and WCOJ/leapfrog
    engine.rs       # Hybrid cyclic/acyclic routing
    query.rs        # TripleStore: indexes, queries, streaming ingest, updates
    export.rs       # N-Triples parser/serializer with literal support
  synthetic.rs      # Graph-shaped synthetic data generator
  sparql.rs         # axum SPARQL server, cache, updates, OPTIONAL
  bin/server.rs     # Standalone server binary
  main.rs           # Local benchmarks and data generation
final_duel.py       # Realistic endpoint duel orchestrator
run_remote_duel.sh  # Hetzner provision + Ansible + duel wrapper
infra/              # Terraform and Ansible automation
bench/              # Archived duel result logs
```

## License

MIT OR Apache-2.0 (choose as convenient).
