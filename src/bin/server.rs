use std::time::Instant;

use trillian::hypertrie::{HybridEngine, TripleStore};
use trillian::sparql::{profile_query, serve, serve_durable};
use trillian::wal::Wal;

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[tokio::main]
async fn main() {
    #[cfg(feature = "dhat")]
    let _dhat = dhat::Profiler::new_heap();

    let args: Vec<String> = std::env::args().collect();

    // Modes (separate loader/server stages):
    //   server build <file.nt> <snapshot.bin>   -> build index + persist
    //   server load  <snapshot.bin> [port]      -> load snapshot via mmap + serve
    //   server <file.nt> [port]                 -> parse + build + serve (default)
    match args.get(1).map(|s| s.as_str()) {
        Some("build") => {
            let nt = args
                .get(2)
                .expect("usage: server build <file.nt> <snapshot.bin>");
            let snap = args
                .get(3)
                .expect("usage: server build <file.nt> <snapshot.bin>");
            build_snapshot(nt, snap);
        }
        Some("load") => {
            let snap = args
                .get(2)
                .expect("usage: server load <snapshot.bin> [port]");
            let port = args.get(3).and_then(|p| p.parse().ok()).unwrap_or(9080);
            load_and_serve(snap, port).await;
        }
        Some("profile") => {
            let nt = args
                .get(2)
                .expect("usage: server profile <file.nt> <query.rq> [runs]");
            let qf = args
                .get(3)
                .expect("usage: server profile <file.nt> <query.rq> [runs]");
            let runs = args.get(4).and_then(|r| r.parse().ok()).unwrap_or(50);
            profile(nt, qf, runs);
        }
        _ => {
            let nt = args.get(1).map(|s| s.as_str()).unwrap_or("synthetic_1m.nt");
            let port = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(9080);
            parse_and_serve(nt, port).await;
        }
    }
}

/// Loader: read the RDF input (.nt or .ttl), build the index, persist it as
/// an mmap snapshot.
fn build_snapshot(nt: &str, snapshot: &str) {
    println!("Building index from {} ...", nt);
    let t0 = Instant::now();
    let mut store = TripleStore::new();
    let n = store
        .ingest_rdf_file(nt)
        .expect("Failed to parse the RDF input file");
    store
        .save_snapshot(snapshot)
        .expect("Failed to write snapshot");
    // A fresh snapshot is the new baseline – any old WAL is obsolete and must
    // not be replayed onto the new snapshot.
    let _ = std::fs::remove_file(format!("{}.wal", snapshot));
    println!(
        "Built + persisted {} triples ({} terms) to {} in {} ms",
        n,
        store.dict.len(),
        snapshot,
        t0.elapsed().as_millis()
    );
}

/// Server: load the snapshot via mmap, replay the WAL, serve durably.
async fn load_and_serve(snapshot: &str, port: u16) {
    println!("Loading snapshot (mmap): {}", snapshot);
    let t0 = Instant::now();
    let mut store = TripleStore::load_snapshot(snapshot).expect("Failed to load snapshot");

    // Replay the WAL after the snapshot (restore durable updates).
    let wal_path = format!("{}.wal", snapshot);
    let replayed = Wal::replay(&wal_path, &mut store).expect("Failed to replay WAL");
    let wal = Wal::open_append(&wal_path).expect("Failed to open WAL");

    println!(
        "Loaded {} triples, {} unique terms ({} WAL ops replayed) in {} ms",
        store.triple_count(),
        store.dict.len(),
        replayed,
        t0.elapsed().as_millis()
    );
    serve_durable(store, port, Some(wal)).await;
}

/// Profiling: build the index in RAM (heap, visible to dhat), print a memory
/// report, and measure a query with phase timing (parse/eval/serialize).
fn profile(nt: &str, query_file: &str, runs: usize) {
    let mut store = TripleStore::new();
    let t0 = Instant::now();
    let n = store
        .ingest_rdf_file(nt)
        .expect("Failed to parse the RDF input file");
    println!(
        "Loaded (in-RAM): {} triples, {} terms in {} ms\n",
        n,
        store.dict.len(),
        t0.elapsed().as_millis()
    );
    store.memory_report();
    println!();
    let engine = HybridEngine::new();
    let query = std::fs::read_to_string(query_file).expect("Failed to read query file");
    profile_query(&store, &engine, &query, runs);
}

/// Default: parse the RDF input (.nt or .ttl), build the index, serve (without
/// a snapshot).
async fn parse_and_serve(nt: &str, port: u16) {
    println!("Loading RDF file: {}", nt);
    let mut store = TripleStore::new();
    let n_triples = store
        .ingest_rdf_file(nt)
        .expect("Failed to parse the RDF input file");
    println!(
        "Ingested {} triples, {} unique terms",
        n_triples,
        store.dict.len()
    );
    serve(store, port).await;
}
