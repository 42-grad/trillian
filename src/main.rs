use std::time::Instant;

use trillian::hypertrie::{
    GraphPattern, HybridEngine, PatternTerm, TriplePattern, TripleStore, export_ntriples,
};

fn main() {
    const NT_FILE: &str = "synthetic_1m.nt";

    // If the file does not exist yet, first generate and export synthetic
    // data (one-time bootstrap).
    if !std::path::Path::new(NT_FILE).exists() {
        log("synthetic_1m.nt not found – generating file...");
        generate_synthetic_nt(NT_FILE);
    }

    // ------------------------------------------------------------------
    // Streaming load + ingest (dictionary + CSR indexes), without keeping
    // the entire parse buffer in memory.
    // ------------------------------------------------------------------
    log("Loading N-Triples file (streaming)...");
    let t0 = Instant::now();
    let mut store = TripleStore::new();
    let n_triples = store
        .ingest_ntriples_file(NT_FILE)
        .expect("Failed to parse .nt file");
    let ingest_time = t0.elapsed();

    println!("=== Ingest Benchmark ===");
    println!("Source file:      {}", NT_FILE);
    println!("Triples:          {}", n_triples);
    println!("Unique terms:     {}", store.dict.len());
    println!("Ingest time:      {} ms", ingest_time.as_millis());
    println!(
        "Ingest throughput: {:.0} triples/sec",
        n_triples as f64 / ingest_time.as_secs_f64()
    );

    // ------------------------------------------------------------------
    // 3. Chain-Join Benchmark (azyklisch)
    // ------------------------------------------------------------------
    // Pattern: (?w, predicate_0, ?x) (?x, predicate_1, ?y) (?y, predicate_2, ?z)
    // A linear chain join over three patterns.
    // ------------------------------------------------------------------
    let pid0 = store
        .dict
        .lookup_iri("http://example.org/predicate_0")
        .unwrap();
    let pid1 = store
        .dict
        .lookup_iri("http://example.org/predicate_1")
        .unwrap();
    let pid2 = store
        .dict
        .lookup_iri("http://example.org/predicate_2")
        .unwrap();

    let chain_pattern = GraphPattern {
        patterns: vec![
            TriplePattern {
                subject: PatternTerm::Variable("w".to_string()),
                predicate: PatternTerm::Bound(pid0),
                object: PatternTerm::Variable("x".to_string()),
            },
            TriplePattern {
                subject: PatternTerm::Variable("x".to_string()),
                predicate: PatternTerm::Bound(pid1),
                object: PatternTerm::Variable("y".to_string()),
            },
            TriplePattern {
                subject: PatternTerm::Variable("y".to_string()),
                predicate: PatternTerm::Bound(pid2),
                object: PatternTerm::Variable("z".to_string()),
            },
        ],
    };

    let engine = HybridEngine::new();

    // Run once to determine the result count
    let chain_results = engine.execute(&store, &chain_pattern).expect("chain query");
    let chain_count = chain_results.n_rows();

    // Benchmark: 1,000 runs
    const N_CHAIN_RUNS: usize = 1_000;
    let t0 = Instant::now();
    for _ in 0..N_CHAIN_RUNS {
        let _ = engine.execute(&store, &chain_pattern);
    }
    let chain_time = t0.elapsed();
    let chain_avg_us = chain_time.as_secs_f64() * 1_000_000.0 / N_CHAIN_RUNS as f64;

    println!("\n=== Chain Join Benchmark ===");
    println!("Pattern:          (?w,p0,?x) (?x,p1,?y) (?y,p2,?z)");
    println!("Runs:             {}", N_CHAIN_RUNS);
    println!(
        "Total time:       {:.2} ms",
        chain_time.as_secs_f64() * 1000.0
    );
    println!("Average time:     {:.2} µs/query", chain_avg_us);
    println!("Result rows:      {}", chain_count);

    // ------------------------------------------------------------------
    // 4. Triangle-Join Benchmark (zyklisch)
    // ------------------------------------------------------------------
    // Pattern: (?a, predicate_0, ?b) (?b, predicate_0, ?c) (?c, predicate_0, ?a)
    // A cyclic triangle join (handled by the WCOJ path).
    // ------------------------------------------------------------------
    let triangle_pattern = GraphPattern {
        patterns: vec![
            TriplePattern {
                subject: PatternTerm::Variable("a".to_string()),
                predicate: PatternTerm::Bound(pid0),
                object: PatternTerm::Variable("b".to_string()),
            },
            TriplePattern {
                subject: PatternTerm::Variable("b".to_string()),
                predicate: PatternTerm::Bound(pid0),
                object: PatternTerm::Variable("c".to_string()),
            },
            TriplePattern {
                subject: PatternTerm::Variable("c".to_string()),
                predicate: PatternTerm::Bound(pid0),
                object: PatternTerm::Variable("a".to_string()),
            },
        ],
    };

    // Run once to determine the result count
    let triangle_results = engine
        .execute(&store, &triangle_pattern)
        .expect("triangle query");
    let triangle_count = triangle_results.n_rows();

    // Benchmark: 20 runs
    const N_TRIANGLE_RUNS: usize = 20;
    let t0 = Instant::now();
    for _ in 0..N_TRIANGLE_RUNS {
        let _ = engine.execute(&store, &triangle_pattern);
    }
    let triangle_time = t0.elapsed();
    let triangle_avg_ms = triangle_time.as_secs_f64() * 1000.0 / N_TRIANGLE_RUNS as f64;

    println!("\n=== Triangle WCOJ Benchmark ===");
    println!("Pattern:          (?a,p0,?b) (?b,p0,?c) (?c,p0,?a)");
    println!("Runs:             {}", N_TRIANGLE_RUNS);
    println!(
        "Total time:       {:.2} ms",
        triangle_time.as_secs_f64() * 1000.0
    );
    println!("Average time:     {:.2} ms/query", triangle_avg_ms);
    println!("Result rows:      {}", triangle_count);
}

fn log(msg: &str) {
    eprintln!("[trillian] {}", msg);
}

fn generate_synthetic_nt(path: &str) {
    use trillian::synthetic::{SyntheticParams, generate};

    // Graph-shaped data with a shared S/O vocabulary and planted
    // triangles/chains, so chain/triangle/star joins return real matches
    // (see src/synthetic.rs).
    let params = SyntheticParams::default();
    let owned = generate(&params);
    let str_triples: Vec<(&str, &str, &str)> = owned
        .iter()
        .map(|(s, p, o)| (s.as_str(), p.as_str(), o.as_str()))
        .collect();

    export_ntriples(path, &str_triples).expect("bootstrap export failed");
    log(&format!(
        "generated {} with {} triples (graph-shaped, {} triangles, {} chains).",
        path, params.n_triples, params.n_triangles, params.n_chains
    ));
}
