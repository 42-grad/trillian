//! Synthetischer, **graph-förmiger** RDF-Datengenerator für das Benchmark-Duell.
//!
//! Frühere Versionen nutzten getrennte Vokabulare (`subject_*` als Subjekt,
//! `object_*` als Objekt). Dadurch konnte ein Objekt nie als Subjekt eines
//! anderen Tripels auftreten – Chain-, Triangle- und Star-Joins matchten
//! **strukturell nie** (0 Ergebniszeilen), und das Duell maß nur den
//! „finde nichts"-Pfad.
//!
//! Dieser Generator verwendet ein **gemeinsames Entitäten-Vokabular** für
//! Subjekt und Objekt und *pflanzt* gezielt Dreiecke und Ketten ein, sodass
//! die Join-Queries deterministisch eine bounded, nicht-leere Treffermenge
//! liefern. Der Rest wird zufällig (seed-fix) aufgefüllt, damit die Indizes
//! realistische Größen haben.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

#[derive(Debug, Clone)]
pub struct SyntheticParams {
    /// Anzahl Entitäten im gemeinsamen S/O-Vokabular.
    pub n_entities: usize,
    /// Anzahl Prädikate.
    pub n_predicates: usize,
    /// Ziel-Anzahl Tripel insgesamt.
    pub n_triples: usize,
    /// Anzahl eingepflanzter Dreiecke auf `predicate_0` (a→b→c→a).
    pub n_triangles: usize,
    /// Anzahl eingepflanzter Ketten w-p0→x-p1→y-p2→z.
    pub n_chains: usize,
    /// RNG-Seed für Reproduzierbarkeit.
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

/// Erzeugt die Tripel als nackte Term-Namen (ohne IRI-Präfix).
///
/// Die ersten `3*n_triangles + 3*n_chains` Tripel sind die gepflanzten
/// Strukturen, danach folgt zufälliger Fülltext.
pub fn generate(params: &SyntheticParams) -> Vec<(String, String, String)> {
    let mut rng = StdRng::seed_from_u64(params.seed);
    let mut triples: Vec<(String, String, String)> = Vec::with_capacity(params.n_triples);

    let p0 = predicate(0);
    let p1 = predicate(1 % params.n_predicates);
    let p2 = predicate(2 % params.n_predicates);

    // Dreiecke auf predicate_0: a→b, b→c, c→a.
    for t in 0..params.n_triangles {
        let a = entity((3 * t) % params.n_entities);
        let b = entity((3 * t + 1) % params.n_entities);
        let c = entity((3 * t + 2) % params.n_entities);
        triples.push((a.clone(), p0.clone(), b.clone()));
        triples.push((b.clone(), p0.clone(), c.clone()));
        triples.push((c, p0.clone(), a));
    }

    // Ketten w-p0→x-p1→y-p2→z, Entitätenblock hinter den Dreiecken.
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

    // Zufälliger Fülltext über das gemeinsame Vokabular.
    while triples.len() < params.n_triples {
        let s = entity(rng.gen_range(0..params.n_entities));
        let p = predicate(rng.gen_range(0..params.n_predicates));
        let o = entity(rng.gen_range(0..params.n_entities));
        triples.push((s, p, o));
    }

    triples
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypertrie::{
        GraphPattern, HybridEngine, PatternTerm, TriplePattern, TripleStore,
    };

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

        let p0 = store.dict.lookup("predicate_0").unwrap();
        let p1 = store.dict.lookup("predicate_1").unwrap();
        let p2 = store.dict.lookup("predicate_2").unwrap();

        // Chain: (?w,p0,?x)(?x,p1,?y)(?y,p2,?z)
        let chain = GraphPattern {
            patterns: vec![
                TriplePattern { subject: var("w"), predicate: PatternTerm::Bound(p0), object: var("x") },
                TriplePattern { subject: var("x"), predicate: PatternTerm::Bound(p1), object: var("y") },
                TriplePattern { subject: var("y"), predicate: PatternTerm::Bound(p2), object: var("z") },
            ],
        };
        let chain_rows = engine.execute(&store, &chain).n_rows();

        // Triangle: (?a,p0,?b)(?b,p0,?c)(?c,p0,?a)
        let triangle = GraphPattern {
            patterns: vec![
                TriplePattern { subject: var("a"), predicate: PatternTerm::Bound(p0), object: var("b") },
                TriplePattern { subject: var("b"), predicate: PatternTerm::Bound(p0), object: var("c") },
                TriplePattern { subject: var("c"), predicate: PatternTerm::Bound(p0), object: var("a") },
            ],
        };
        let triangle_rows = engine.execute(&store, &triangle).n_rows();

        assert!(chain_rows > 0, "chain join must be non-empty, got {}", chain_rows);
        assert!(
            triangle_rows > 0,
            "triangle join must be non-empty, got {}",
            triangle_rows
        );
    }
}
