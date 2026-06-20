use std::time::Instant;

use tentris_clone::hypertrie::TripleStore;
use tentris_clone::sparql::serve;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Modi (analog zu Tentris' loader/server-Trennung):
    //   server build <file.nt> <snapshot.bin>   -> Index bauen + persistieren
    //   server load  <snapshot.bin> [port]      -> Snapshot per mmap laden + serven
    //   server <file.nt> [port]                 -> parsen + bauen + serven (Default)
    match args.get(1).map(|s| s.as_str()) {
        Some("build") => {
            let nt = args.get(2).expect("usage: server build <file.nt> <snapshot.bin>");
            let snap = args.get(3).expect("usage: server build <file.nt> <snapshot.bin>");
            build_snapshot(nt, snap);
        }
        Some("load") => {
            let snap = args.get(2).expect("usage: server load <snapshot.bin> [port]");
            let port = args.get(3).and_then(|p| p.parse().ok()).unwrap_or(9080);
            load_and_serve(snap, port).await;
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
    store.save_snapshot(snapshot).expect("Failed to write snapshot");
    println!(
        "Built + persisted {} triples ({} terms) to {} in {} ms",
        n,
        store.dict.len(),
        snapshot,
        t0.elapsed().as_millis()
    );
}

/// Server: Snapshot per mmap laden und SPARQL-Endpoint starten.
async fn load_and_serve(snapshot: &str, port: u16) {
    println!("Loading snapshot (mmap): {}", snapshot);
    let t0 = Instant::now();
    let store = TripleStore::load_snapshot(snapshot).expect("Failed to load snapshot");
    println!(
        "Loaded {} triples, {} unique terms in {} ms",
        store.triple_count(),
        store.dict.len(),
        t0.elapsed().as_millis()
    );
    serve(store, port).await;
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
