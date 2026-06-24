//! Synthetic, **graph-shaped** RDF data generator for the benchmark.
//!
//! Earlier versions used separate vocabularies (`subject_*` as subject,
//! `object_*` as object). That meant an object could never appear as the
//! subject of another triple — chain, triangle, and star joins **never matched
//! structurally** (0 result rows), and the benchmark only measured the
//! "find nothing" path.
//!
//! This generator uses a **shared entity vocabulary** for subject and object
//! and deliberately *plants* triangles and chains so the join queries
//! deterministically return a bounded, non-empty result set. The rest is filled
//! in randomly (fixed seed) so the indexes have realistic sizes.

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

#[derive(Debug, Clone)]
pub struct SyntheticParams {
    /// Number of entities in the shared S/O vocabulary.
    pub n_entities: usize,
    /// Number of predicates.
    pub n_predicates: usize,
    /// Target total number of triples.
    pub n_triples: usize,
    /// Number of planted triangles on `predicate_0` (a→b→c→a).
    pub n_triangles: usize,
    /// Number of planted chains w-p0→x-p1→y-p2→z.
    pub n_chains: usize,
    /// RNG seed for reproducibility.
    pub seed: u64,
}

impl Default for SyntheticParams {
    fn default() -> Self {
        Self {
            n_entities: 50_000,
            n_predicates: 100,
            n_triples: 1_000_000,
            n_triangles: 1_000,
            n_chains: 2_000,
            seed: 42,
        }
    }
}

fn entity(i: usize) -> String {
    format!("entity_{}", i)
}

fn predicate(i: usize) -> String {
    format!("predicate_{}", i)
}

/// Generates the triples as bare term names (without an IRI prefix).
///
/// The first `3*n_triangles + 3*n_chains` triples are the planted structures,
/// followed by random filler.
pub fn generate(params: &SyntheticParams) -> Vec<(String, String, String)> {
    let mut rng = StdRng::seed_from_u64(params.seed);
    let mut triples: Vec<(String, String, String)> = Vec::with_capacity(params.n_triples);

    let p0 = predicate(0);
    let p1 = predicate(1 % params.n_predicates);
    let p2 = predicate(2 % params.n_predicates);

    // Triangles on predicate_0: a→b, b→c, c→a.
    for t in 0..params.n_triangles {
        let a = entity((3 * t) % params.n_entities);
        let b = entity((3 * t + 1) % params.n_entities);
        let c = entity((3 * t + 2) % params.n_entities);
        triples.push((a.clone(), p0.clone(), b.clone()));
        triples.push((b.clone(), p0.clone(), c.clone()));
        triples.push((c, p0.clone(), a));
    }

    // Chains w-p0→x-p1→y-p2→z, entity block after the triangles.
    let base = 3 * params.n_triangles;
    for c in 0..params.n_chains {
        let w = entity((base + 4 * c) % params.n_entities);
        let x = entity((base + 4 * c + 1) % params.n_entities);
        let y = entity((base + 4 * c + 2) % params.n_entities);
        let z = entity((base + 4 * c + 3) % params.n_entities);
        triples.push((w, p0.clone(), x.clone()));
        triples.push((x, p1.clone(), y.clone()));
        triples.push((y, p2.clone(), z));
    }

    // Random filler over the shared vocabulary.
    while triples.len() < params.n_triples {
        let s = entity(rng.random_range(0..params.n_entities));
        let p = predicate(rng.random_range(0..params.n_predicates));
        let o = entity(rng.random_range(0..params.n_entities));
        triples.push((s, p, o));
    }

    triples
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypertrie::{GraphPattern, HybridEngine, PatternTerm, TriplePattern, TripleStore};

    fn var(name: &str) -> PatternTerm {
        PatternTerm::Variable(name.to_string())
    }

    #[test]
    fn graph_data_produces_non_empty_joins() {
        let params = SyntheticParams {
            n_entities: 2_000,
            n_predicates: 10,
            n_triples: 30_000,
            n_triangles: 50,
            n_chains: 50,
            seed: 7,
        };
        let triples = generate(&params);
        let str_triples: Vec<(&str, &str, &str)> = triples
            .iter()
            .map(|(s, p, o)| (s.as_str(), p.as_str(), o.as_str()))
            .collect();

        let mut store = TripleStore::new();
        store.ingest_str_triples(&str_triples);
        let engine = HybridEngine::new();

        let p0 = store.dict.lookup_iri("predicate_0").unwrap();
        let p1 = store.dict.lookup_iri("predicate_1").unwrap();
        let p2 = store.dict.lookup_iri("predicate_2").unwrap();

        // Chain: (?w,p0,?x)(?x,p1,?y)(?y,p2,?z)
        let chain = GraphPattern {
            patterns: vec![
                TriplePattern {
                    subject: var("w"),
                    predicate: PatternTerm::Bound(p0),
                    object: var("x"),
                },
                TriplePattern {
                    subject: var("x"),
                    predicate: PatternTerm::Bound(p1),
                    object: var("y"),
                },
                TriplePattern {
                    subject: var("y"),
                    predicate: PatternTerm::Bound(p2),
                    object: var("z"),
                },
            ],
        };
        let chain_rows = engine.execute(&store, &chain).expect("chain").n_rows();

        // Triangle: (?a,p0,?b)(?b,p0,?c)(?c,p0,?a)
        let triangle = GraphPattern {
            patterns: vec![
                TriplePattern {
                    subject: var("a"),
                    predicate: PatternTerm::Bound(p0),
                    object: var("b"),
                },
                TriplePattern {
                    subject: var("b"),
                    predicate: PatternTerm::Bound(p0),
                    object: var("c"),
                },
                TriplePattern {
                    subject: var("c"),
                    predicate: PatternTerm::Bound(p0),
                    object: var("a"),
                },
            ],
        };
        let triangle_rows = engine
            .execute(&store, &triangle)
            .expect("triangle")
            .n_rows();

        assert!(
            chain_rows > 0,
            "chain join must be non-empty, got {}",
            chain_rows
        );
        assert!(
            triangle_rows > 0,
            "triangle join must be non-empty, got {}",
            triangle_rows
        );
    }
}
