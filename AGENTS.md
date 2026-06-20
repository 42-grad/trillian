# Agent Notes

## Build & Test

```bash
cargo check --bin server
cargo test
cargo build --release --bin server
```

## Local Smoke Test

```bash
./target/release/server /tmp/smoke.nt 9999
# In another shell:
curl "http://localhost:9999/sparql?query=..."
curl "http://localhost:9999/stream?query=..."
curl "http://localhost:9999/count?query=..."
curl -X POST "http://localhost:9999/update" -H "Content-Type: application/sparql-update" --data 'INSERT DATA { ... }'
```

## Conventions

- Flat vectors and `u32` IDs; avoid pointer chasing.
- Use `rustc_hash::FxHashSet`/`FxHashMap` for hot paths.
- Keep SPARQL support minimal and incremental; unsupported algebra should return a clear error.
- Run `cargo test` after every semantic change.
- The remote duel requires `HCLOUD_TOKEN`; it costs ~0.12–0.16 € per run.

## Key Files

- `src/hypertrie/dictionary.rs`: term types and dictionary.
- `src/hypertrie/planner.rs`: `GraphPattern`, `TriplePattern`, `PatternTerm`.
- `src/hypertrie/engine.rs`: hybrid cyclic/acyclic dispatch.
- `src/sparql.rs`: HTTP endpoints, OPTIONAL, cache, updates.
- `final_duel.py`: endpoint benchmark orchestrator.
- `run_remote_duel.sh`: full Hetzner duel workflow.
