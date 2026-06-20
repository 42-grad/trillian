use std::path::PathBuf;

use tentris_clone::hypertrie::TripleStore;
use tentris_clone::sparql::serve_with_persistence;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    let (nt_file, port) = match args.len() {
        1 => (PathBuf::from("synthetic_1m.nt"), 9080),
        2 => (PathBuf::from(&args[1]), 9080),
        _ => (PathBuf::from(&args[1]), args[2].parse().unwrap_or(9080)),
    };

    println!("Loading N-Triples file: {}", nt_file.display());
    let mut store = TripleStore::new();
    // Streamendes Laden: nie den gesamten Parse-Puffer im Speicher halten.
    let n_triples = store
        .ingest_ntriples_file(nt_file.to_str().unwrap())
        .expect("Failed to parse .nt file");
    println!(
        "Ingested {} triples, {} unique terms",
        n_triples,
        store.dict.len()
    );

    // Opt-in Write-Through-Persistenz: mit TENTRIS_PERSIST=1 schreibt der
    // Server nach jedem /update den Store verlustfrei in die Eingabedatei
    // zurück, sodass Änderungen einen Neustart überleben.
    let persist_path = match std::env::var("TENTRIS_PERSIST") {
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => {
            println!("Persistence enabled (write-through to {})", nt_file.display());
            Some(nt_file.clone())
        }
        _ => None,
    };

    serve_with_persistence(store, port, persist_path).await;
}
