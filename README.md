# Tentris Clone in Rust

A high-performance RDF triple store and SPARQL endpoint written in Rust, inspired by the DICE-group research prototype [Tentris](https://github.com/dice-group/tentris).

## Goal

Build a lean, cache-friendly graph database that can run realistic SPARQL-over-HTTP benchmarks against the original C++ Tentris on identical data and queries.

## Architecture

- **Dictionary**: String ↔ `u32` term mapping with term-type metadata (IRI, literal with datatype/language, blank node).
- **LayeredIndex / CSR**: Three-level compressed sparse row structure storing SPO/POS/OSP permutations as flat contiguous vectors.
- **Hybrid Query Engine**:
  - Acyclic BGPs: cost-based left-deep planner with dictionary statistics.
  - Cyclic BGPs: WCOJ (Worst-Case Optimal Join) via predicate-specific 2D CSR relations and leapfrog intersection.
- **SPARQL Endpoint**: axum-based HTTP server exposing `/sparql`, `/stream`, `/count`, and `/update`.
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

### Latest Result (Forschungs-Tentris, Ubuntu 24.04, cpx42)

| Metrik / Benchmark | Rust SPARQL-Endpoint | C++ Tentris SPARQL-Endpoint | Faktor |
| :--- | :--- | :--- | :--- |
| **Ingest (1M Triples)** | 1205.20 ms | 6425.14 ms | Rust ~5.3× schneller |
| **Azyklischer Chain-Join** | 654.19 µs/query (0 rows) | 2708.24 µs/query (0 rows) | Rust ~4.1× schneller |
| **Zyklischer Triangle-Join (WCOJ)** | 0.67 ms/query (0 rows) | 2.77 ms/query (0 rows) | Rust ~4.1× schneller |

Konsistenzprüfung (Result Rows):
- Chain: Rust=0, Tentris=0 → OK
- Triangle: Rust=0, Tentris=0 → OK

Both queries return zero rows on the synthetic dataset by construction; the benchmark still measures full plan optimization and execution consistently.

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
    index.rs        # 3-level CSR LayeredIndex
    stats.rs        # Cardinality statistics
    planner.rs      # BGP optimizer and GraphPattern
    executor.rs     # Plan executor and WCOJ/leapfrog
    engine.rs       # Hybrid cyclic/acyclic routing
    relation.rs     # Per-predicate 2D CSR
    query.rs        # Low-level triple/merge queries
    export.rs       # N-Triples parser with literal support
  sparql.rs         # axum SPARQL server, cache, updates, OPTIONAL
  bin/server.rs     # Standalone server binary
  main.rs           # Local benchmarks and data generation
final_duel.py       # Realistic endpoint duel orchestrator
run_remote_duel.sh  # Hetzner provision + Ansible + duel wrapper
infra/              # Terraform and Ansible automation
```

## License

MIT OR Apache-2.0 (choose as convenient).
