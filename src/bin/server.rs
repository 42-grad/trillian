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

    // Modi (analog zu Tentris' loader/server-Trennung):
    //   server build <file.nt> <snapshot.bin>   -> Index bauen + persistieren
    //   server load  <snapshot.bin> [port]      -> Snapshot per mmap laden + serven
    //   server <file.nt> [port]                 -> parsen + bauen + serven (Default)
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

/// Loader: N-Triples einlesen, Index bauen, als mmap-Snapshot persistieren.
fn build_snapshot(nt: &str, snapshot: &str) {
    println!("Building index from {} ...", nt);
    let t0 = Instant::now();
    let mut store = TripleStore::new();
    let n = store
        .ingest_ntriples_file(nt)
        .expect("Failed to parse .nt file");
    store
        .save_snapshot(snapshot)
        .expect("Failed to write snapshot");
    // Ein frischer Snapshot ist die neue Baseline – ein evtl. altes WAL ist
    // obsolet und darf nicht auf den neuen Snapshot zurückgespielt werden.
    let _ = std::fs::remove_file(format!("{}.wal", snapshot));
    println!(
        "Built + persisted {} triples ({} terms) to {} in {} ms",
        n,
        store.dict.len(),
        snapshot,
        t0.elapsed().as_millis()
    );
}

/// Server: Snapshot per mmap laden, WAL zurückspielen, durabel serven.
async fn load_and_serve(snapshot: &str, port: u16) {
    println!("Loading snapshot (mmap): {}", snapshot);
    let t0 = Instant::now();
    let mut store = TripleStore::load_snapshot(snapshot).expect("Failed to load snapshot");

    // WAL nach dem Snapshot zurückspielen (durable Updates wiederherstellen).
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

/// Profiling: Index in-RAM bauen (Heap, für dhat sichtbar), Memory-Report
/// drucken und eine Query mit Phasen-Timing (Parse/Eval/Serialize) messen.
fn profile(nt: &str, query_file: &str, runs: usize) {
    let mut store = TripleStore::new();
    let t0 = Instant::now();
    let n = store
        .ingest_ntriples_file(nt)
        .expect("Failed to parse .nt file");
    println!(
        "Geladen (in-RAM): {} Triples, {} Terme in {} ms\n",
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

/// Default: N-Triples parsen, Index bauen, serven (ohne Snapshot).
async fn parse_and_serve(nt: &str, port: u16) {
    println!("Loading N-Triples file: {}", nt);
    let mut store = TripleStore::new();
    let n_triples = store
        .ingest_ntriples_file(nt)
        .expect("Failed to parse .nt file");
    println!(
        "Ingested {} triples, {} unique terms",
        n_triples,
        store.dict.len()
    );
    serve(store, port).await;
}
