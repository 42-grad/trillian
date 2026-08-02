//! End-to-end integration tests exercising the public crate API: build a store,
//! run SPARQL through `execute_sparql` (the same path the HTTP endpoint uses),
//! and round-trip through a memory-mapped snapshot.

use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;
use trillian::hypertrie::{HybridEngine, TripleStore};
use trillian::sparql::{execute_sparql, execute_sparql_bind};

const EX: &str = "http://example.org/";

/// A small social graph: alice -> bob -> charlie -> alice (knows), plus a typed
/// integer age. Built from N-Triples on disk so the example also exercises the
/// streaming loader and a typed literal (needed for the numeric FILTER test).
fn social_store() -> TripleStore {
    let nt = format!(
        "<{EX}alice> <{EX}knows> <{EX}bob> .\n\
         <{EX}bob> <{EX}knows> <{EX}charlie> .\n\
         <{EX}charlie> <{EX}knows> <{EX}alice> .\n\
         <{EX}bob> <{EX}age> \"25\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n"
    );
    let path = unique_path("graph.nt");
    std::fs::write(&path, nt).unwrap();
    let mut store = TripleStore::new();
    store.ingest_ntriples_file(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    store
}

/// Parse a SPARQL-results JSON body and return the `bindings` array.
fn bindings(json: &str) -> Vec<Value> {
    let v: Value = serde_json::from_str(json).expect("valid SPARQL-results JSON");
    v["results"]["bindings"]
        .as_array()
        .expect("bindings array")
        .clone()
}

fn unique_path(tag: &str) -> std::path::PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("trillian_it_{}_{tag}_{n}.bin", std::process::id()))
}

#[test]
fn select_multi_pattern_join() {
    let store = social_store();
    let engine = HybridEngine::new();
    // Two-pattern path join: who does ?a know whose acquaintance is ?c.
    let q = format!("SELECT ?a ?c WHERE {{ ?a <{EX}knows> ?b . ?b <{EX}knows> ?c }}");
    let rows = bindings(&execute_sparql(&store, &engine, &q).unwrap());
    // alice->bob->charlie, bob->charlie->alice, charlie->alice->bob = 3 rows.
    assert_eq!(rows.len(), 3);
}

#[test]
fn select_optional_keeps_unmatched_rows() {
    let store = social_store();
    let engine = HybridEngine::new();
    let q =
        format!("SELECT ?a ?age WHERE {{ ?a <{EX}knows> ?b . OPTIONAL {{ ?b <{EX}age> ?age }} }}");
    let rows = bindings(&execute_sparql(&store, &engine, &q).unwrap());
    // 3 knows-edges; only the edge into bob has an age, the others stay (NULL).
    assert_eq!(rows.len(), 3);
    let with_age = rows.iter().filter(|r| r.get("age").is_some()).count();
    assert_eq!(with_age, 1);
}

#[test]
fn select_filter_numeric() {
    let store = social_store();
    let engine = HybridEngine::new();
    let q = format!("SELECT ?b ?age WHERE {{ ?b <{EX}age> ?age FILTER(?age > 20) }}");
    let rows = bindings(&execute_sparql(&store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["age"]["value"], "25");
}

#[test]
fn transitive_property_path() {
    let store = social_store();
    let engine = HybridEngine::new();
    // knows+ from alice reaches everyone in the cycle.
    let q = format!("SELECT ?o WHERE {{ <{EX}alice> <{EX}knows>+ ?o }}");
    let rows = bindings(&execute_sparql(&store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 3); // bob, charlie, alice (via the cycle)
}

#[test]
fn ask_query() {
    let store = social_store();
    let engine = HybridEngine::new();
    let yes = format!("ASK {{ <{EX}alice> <{EX}knows> <{EX}bob> }}");
    let no = format!("ASK {{ <{EX}alice> <{EX}knows> <{EX}charlie> }}");
    let yv: Value = serde_json::from_str(&execute_sparql(&store, &engine, &yes).unwrap()).unwrap();
    let nv: Value = serde_json::from_str(&execute_sparql(&store, &engine, &no).unwrap()).unwrap();
    assert_eq!(yv["boolean"], Value::Bool(true));
    assert_eq!(nv["boolean"], Value::Bool(false));
}

#[test]
fn snapshot_roundtrip_then_query() {
    // Full path: ingest -> save snapshot -> mmap load -> SPARQL on the loaded store.
    let mut store = social_store();
    let path = unique_path("snap");
    store.save_snapshot(path.to_str().unwrap()).unwrap();

    let loaded = TripleStore::load_snapshot(path.to_str().unwrap()).unwrap();
    assert_eq!(loaded.triple_count(), store.triple_count());

    let engine = HybridEngine::new();
    let q = format!("SELECT ?o WHERE {{ <{EX}alice> <{EX}knows> ?o }}");
    let rows = bindings(&execute_sparql(&loaded, &engine, &q).unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["o"]["value"], format!("{EX}bob"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_snapshot_corrupt_file_errors() {
    // The hardened loader returns an error (does not panic) on garbage input.
    let path = unique_path("corrupt");
    std::fs::write(&path, vec![0u8; 50]).unwrap();
    assert!(TripleStore::load_snapshot(path.to_str().unwrap()).is_err());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn group_by_count_star() {
    let mut store = social_store();
    let engine = HybridEngine::new();
    let q = format!("SELECT ?s (COUNT(*) AS ?cnt) WHERE {{ ?s <{EX}knows> ?o }} GROUP BY ?s");
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    // alice, bob, charlie each have exactly one outgoing knows edge.
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row["cnt"]["value"], "1");
        assert_eq!(row["cnt"]["type"], "literal");
        assert_eq!(
            row["cnt"]["datatype"],
            "http://www.w3.org/2001/XMLSchema#integer"
        );
    }
}

#[test]
fn group_by_count_with_having() {
    let mut store = social_store();
    let engine = HybridEngine::new();
    let q = format!(
        "SELECT ?s (COUNT(*) AS ?cnt) WHERE {{ ?s <{EX}knows> ?o }} GROUP BY ?s HAVING (COUNT(*) > 0)"
    );
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 3);
}

#[test]
fn group_by_count_order_by_aggregate() {
    let mut store = social_store();
    let engine = HybridEngine::new();
    let q = format!(
        "SELECT ?s (COUNT(*) AS ?cnt) WHERE {{ ?s <{EX}knows> ?o }} GROUP BY ?s ORDER BY DESC(?cnt)"
    );
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 3);
    // All counts are 1; the main assertion is that ordering by the aggregate
    // variable does not panic and returns a stable result.
    for row in &rows {
        assert_eq!(row["cnt"]["value"], "1");
    }
}

#[test]
fn group_by_count_empty_group_returns_zero() {
    let mut store = TripleStore::new();
    let engine = HybridEngine::new();
    let q = format!("SELECT (COUNT(*) AS ?cnt) WHERE {{ ?s <{EX}knows> ?o }}");
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    // SPARQL 1.1: without GROUP BY there is always one (empty) group, so an
    // aggregate-only query over no solutions yields one row with COUNT = 0.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["cnt"]["value"], "0");
    assert_eq!(
        rows[0]["cnt"]["datatype"],
        "http://www.w3.org/2001/XMLSchema#integer"
    );
}

#[test]
fn count_distinct_star_dedupes_within_group() {
    let mut store = social_store();
    let engine = HybridEngine::new();
    // Overlapping UNION branches produce duplicate full solutions within a
    // group; COUNT(*) must see both, COUNT(DISTINCT *) only one.
    let q = format!(
        "SELECT ?s (COUNT(*) AS ?c_all) (COUNT(DISTINCT *) AS ?c_dist) WHERE {{ \
         {{ ?s <{EX}knows> ?o }} UNION {{ ?s <{EX}knows> ?o }} }} GROUP BY ?s"
    );
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row["c_all"]["value"], "2");
        assert_eq!(row["c_dist"]["value"], "1");
    }
}
