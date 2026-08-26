//! End-to-end integration tests exercising the public crate API: build a store,
//! run SPARQL through `execute_sparql` (the same path the HTTP endpoint uses),
//! and round-trip through a memory-mapped snapshot.

use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;
use trillian::hypertrie::{HybridEngine, TripleStore};
use trillian::sparql::{execute_sparql, execute_sparql_bind};

const EX: &str = "http://example.org/";
const XSD_INT: &str = "http://www.w3.org/2001/XMLSchema#integer";

/// Generic store creation helper: write the given N-Triples to a temporary file, ingest it into a new store, and return the store.
/// Built from N-Triples on disk so the example also exercises the
/// streaming loader and a typed literal (needed for the numeric FILTER test).
fn store_from_nt(nt: &str) -> TripleStore {
    let path = unique_path("graph.nt");
    std::fs::write(&path, nt).unwrap();
    let mut store = TripleStore::new();
    store.ingest_ntriples_file(path.to_str().unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    store
}

/// A small social graph: alice -> bob -> charlie -> alice (knows), plus a typed integer age.
fn social_store() -> TripleStore {
    store_from_nt(&format!(
        "<{EX}alice> <{EX}knows> <{EX}bob> .\n\
         <{EX}bob> <{EX}knows> <{EX}charlie> .\n\
         <{EX}charlie> <{EX}knows> <{EX}alice> .\n\
         <{EX}bob> <{EX}age> \"25\"^^<{XSD_INT}> .\n"
    ))
}

/// A small store with multiple scores per subject in order to test aggregate functions.
fn score_store() -> TripleStore {
    store_from_nt(&format!(
        "<{EX}alice> <{EX}score> \"7\"^^<{XSD_INT}> .\n\
         <{EX}alice> <{EX}score> \"10\"^^<{XSD_INT}> .\n\
         <{EX}alice> <{EX}score> \"9\"^^<{XSD_INT}> .\n\
         <{EX}bob> <{EX}score> \"8\"^^<{XSD_INT}> .\n\
         <{EX}bob> <{EX}score> \"5\"^^<{XSD_INT}> .\n"
    ))
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
        assert_eq!(row["cnt"]["datatype"], XSD_INT);
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
    assert_eq!(rows[0]["cnt"]["datatype"], XSD_INT);
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

// --- Aggregate function tests ----------------------------------------------

/// Runs `SELECT (AGG(?score) AS ?out)` with no GROUP BY; one implicit group
/// over every solution and asserts the single result row.
fn assert_global_agg(agg: &str, expected: &str) {
    let mut store = score_store();
    let engine = HybridEngine::new();
    let q = format!("SELECT ({agg}(?score) AS ?out) WHERE {{ ?s <{EX}score> ?score }}");
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 1, "no GROUP BY yields exactly one group");
    // Must be the stored term, not a value re-interned as xsd:double.
    assert_eq!(rows[0]["out"]["datatype"], XSD_INT);
    assert_eq!(
        rows[0]["out"]["value"], expected,
        "{agg} over the whole store"
    );
}

/// Runs `SELECT ?s (AGG(?v) AS ?out) ... GROUP BY ?s` and checks each group's
/// result against the values that group is allowed to produce. MIN/MAX pass a
/// single permitted value; SAMPLE passes the whole group, since SPARQL leaves
/// its choice unspecified. Group order is unspecified too (groups come out of a
/// hash map), so rows are looked up by subject rather than by position.
fn assert_grouped_agg(agg: &str, expected: &[(&str, &[&str])]) {
    let mut store = score_store();
    let engine = HybridEngine::new();
    let q = format!("SELECT ?s ({agg}(?v) AS ?out) WHERE {{ ?s <{EX}score> ?v }} GROUP BY ?s");
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(rows.len(), expected.len(), "one row per grouped subject");

    let got: std::collections::HashMap<&str, &str> = rows
        .iter()
        .map(|r| {
            (
                r["s"]["value"].as_str().unwrap(),
                r["out"]["value"].as_str().unwrap(),
            )
        })
        .collect();
    for (subject, allowed) in expected {
        let v = got[format!("{EX}{subject}").as_str()];
        assert!(
            allowed.contains(&v),
            "{agg} for ?s = {subject} returned {v}, expected one of {allowed:?}"
        );
    }
}

#[test]
fn select_min_global() {
    assert_global_agg("MIN", "5");
}

#[test]
fn select_max_global() {
    assert_global_agg("MAX", "10");
}

#[test]
fn select_min_within_group() {
    assert_grouped_agg("MIN", &[("alice", &["7"]), ("bob", &["5"])]);
}

#[test]
fn select_max_within_group() {
    assert_grouped_agg("MAX", &[("alice", &["10"]), ("bob", &["8"])]);
}

#[test]
fn select_sample_within_group() {
    // Any member of the group is a conformant answer.
    assert_grouped_agg(
        "SAMPLE",
        &[("alice", &["7", "10", "9"]), ("bob", &["8", "5"])],
    );
}

/// A group whose rows all leave the aggregate variable unbound must yield an
/// unbound result rather than a bogus term, i.e. `?out` is omitted from those
/// bindings. Only bob has an age, so of the three knows-edges only alice's
/// group aggregates over anything; bob's and charlie's groups have a row but
/// nothing bound in it.
fn assert_unbound_group_agg(agg: &str) {
    let mut store = social_store();
    let engine = HybridEngine::new();
    let q = format!(
        "SELECT ?a ({agg}(?age) AS ?out) WHERE {{ ?a <{EX}knows> ?b \
         OPTIONAL {{ ?b <{EX}age> ?age }} }} GROUP BY ?a"
    );
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 3, "one row per grouped subject, bound or not");
    for row in &rows {
        let a = row["a"]["value"].as_str().unwrap();
        if a == format!("{EX}alice") {
            assert_eq!(
                row["out"]["value"], "25",
                "{agg}: alice knows bob, who has an age"
            );
        } else {
            assert!(
                row["out"].is_null(),
                "{agg}: ?out must be omitted for {a}, got {}",
                row["out"]
            );
        }
    }
}

/// With no solutions and no GROUP BY there is still one implicit group, and
/// these aggregates are unbound over it - unlike COUNT, which yields 0.
fn assert_empty_group_agg(agg: &str) {
    let mut store = social_store();
    let engine = HybridEngine::new();
    let q = format!("SELECT ({agg}(?v) AS ?out) WHERE {{ ?s <{EX}missing> ?v }}");
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(
        rows.len(),
        1,
        "the implicit group survives with no solutions"
    );
    assert!(
        rows[0]["out"].is_null(),
        "{agg} over an empty group is unbound"
    );
}

#[test]
fn aggregate_over_unbound_group_is_unbound() {
    for agg in ["MIN", "MAX", "SAMPLE"] {
        assert_unbound_group_agg(agg);
    }
}

#[test]
fn aggregate_over_empty_group_is_unbound() {
    for agg in ["MIN", "MAX", "SAMPLE"] {
        assert_empty_group_agg(agg);
    }
}

/// A group mixing bound and unbound rows must aggregate over the bound ones
/// only. This is the case that pins the `NULL_ID` filter: an unbound row sorts
/// below every real term in the ORDER BY ordering, so without the filter MIN
/// would hand back the unbound row instead of the smallest actual value.
fn assert_mixed_group_agg(agg: &str, expected: &[&str]) {
    // alice knows two people, but only bob has an age - so alice's group has
    // one bound row and one unbound row.
    let mut store = store_from_nt(&format!(
        "<{EX}alice> <{EX}knows> <{EX}bob> .\n\
         <{EX}alice> <{EX}knows> <{EX}dave> .\n\
         <{EX}bob> <{EX}age> \"25\"^^<{XSD_INT}> .\n"
    ));
    let engine = HybridEngine::new();
    let q = format!(
        "SELECT ?a ({agg}(?age) AS ?out) WHERE {{ ?a <{EX}knows> ?b \
         OPTIONAL {{ ?b <{EX}age> ?age }} }} GROUP BY ?a"
    );
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 1, "one group: alice");
    let v = rows[0]["out"]["value"].as_str().unwrap_or_else(|| {
        panic!(
            "{agg}: ?out must be bound, the group has a bound row; got {}",
            rows[0]["out"]
        )
    });
    assert!(
        expected.contains(&v),
        "{agg} over a mixed group returned {v}, expected one of {expected:?}"
    );
}

#[test]
fn aggregate_over_mixed_group_ignores_unbound_rows() {
    for (agg, expected) in [
        ("MIN", &["25"][..]),
        ("MAX", &["25"][..]),
        ("SAMPLE", &["25"][..]),
    ] {
        assert_mixed_group_agg(agg, expected);
    }
}

#[test]
fn count_var_ignores_unbound_rows() {
    // alice knows two people but only bob has an age, so the OPTIONAL leaves
    // one row unbound.
    let mut store = store_from_nt(&format!(
        "<{EX}alice> <{EX}knows> <{EX}bob> .\n\
         <{EX}alice> <{EX}knows> <{EX}dave> .\n\
         <{EX}bob> <{EX}age> \"25\"^^<{XSD_INT}> .\n"
    ));
    let engine = HybridEngine::new();
    let q = format!(
        "SELECT (COUNT(*) AS ?rows) (COUNT(?age) AS ?ages) WHERE {{ \
         <{EX}alice> <{EX}knows> ?b OPTIONAL {{ ?b <{EX}age> ?age }} }}"
    );
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["rows"]["value"], "2",
        "COUNT(*) counts both solutions"
    );
    assert_eq!(
        rows[0]["ages"]["value"], "1",
        "COUNT(?age) counts only bob's"
    );
    assert_eq!(rows[0]["ages"]["datatype"], XSD_INT);
}

#[test]
fn count_distinct_var_dedupes_within_group() {
    let mut store = social_store();
    let engine = HybridEngine::new();
    // Overlapping UNION branches bind ?o to the same term twice per group.
    let q = format!(
        "SELECT ?s (COUNT(?o) AS ?c_all) (COUNT(DISTINCT ?o) AS ?c_dist) WHERE {{ \
         {{ ?s <{EX}knows> ?o }} UNION {{ ?s <{EX}knows> ?o }} }} GROUP BY ?s"
    );
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row["c_all"]["value"], "2", "both bindings counted");
        assert_eq!(
            row["c_dist"]["value"], "1",
            "the two bindings are the same term"
        );
    }
}

#[test]
fn count_var_over_empty_group_is_zero() {
    let mut store = social_store();
    let engine = HybridEngine::new();
    // Unlike MIN/MAX/SAMPLE, COUNT is *bound* over an empty group.
    let q = format!("SELECT (COUNT(?v) AS ?c) WHERE {{ ?s <{EX}missing> ?v }}");
    let rows = bindings(&execute_sparql_bind(&mut store, &engine, &q).unwrap());
    assert_eq!(
        rows.len(),
        1,
        "the implicit group survives with no solutions"
    );
    assert_eq!(rows[0]["c"]["value"], "0");
    assert_eq!(rows[0]["c"]["datatype"], XSD_INT);
}
