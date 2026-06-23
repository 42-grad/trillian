use rustc_hash::FxHashMap;

use super::planner::PatternTerm;

/// Cardinality primitives for the planner. Implemented both by [`Stats`] (tests,
/// precomputed maps) and by `TripleStore` (on demand from the index, without
/// memory-hungry pair-count maps).
pub trait CardEstimator {
    fn total(&self) -> usize;
    /// #objects for (s, p)
    fn sp(&self, s: u32, p: u32) -> usize;
    /// #subjects for (p, o)
    fn po(&self, p: u32, o: u32) -> usize;
    /// #predicates for (o, s)
    fn os(&self, o: u32, s: u32) -> usize;
    /// #triples with subject s
    fn sdeg(&self, s: u32) -> usize;
    /// #triples with predicate p
    fn pdeg(&self, p: u32) -> usize;
    /// #triples with object o
    fn odeg(&self, o: u32) -> usize;
}

/// Estimates the cardinality (number of result rows) of a triple pattern via
/// any [`CardEstimator`].
pub fn estimate_cardinality<E: CardEstimator + ?Sized>(
    est: &E,
    s: &PatternTerm,
    p: &PatternTerm,
    o: &PatternTerm,
) -> usize {
    use PatternTerm::{Bound, Variable};
    match (s, p, o) {
        (Bound(_), Bound(_), Bound(_)) => 1,
        (Bound(sv), Bound(pv), Variable(_)) => est.sp(*sv, *pv),
        (Variable(_), Bound(pv), Bound(ov)) => est.po(*pv, *ov),
        (Bound(sv), Variable(_), Bound(ov)) => est.os(*ov, *sv),
        (Bound(sv), Variable(_), Variable(_)) => est.sdeg(*sv),
        (Variable(_), Bound(pv), Variable(_)) => est.pdeg(*pv),
        (Variable(_), Variable(_), Bound(ov)) => est.odeg(*ov),
        (Variable(_), Variable(_), Variable(_)) => est.total(),
    }
}

impl CardEstimator for Stats {
    fn total(&self) -> usize {
        self.total_triples
    }
    fn sp(&self, s: u32, p: u32) -> usize {
        self.spo_pair_count.get(&(s, p)).copied().unwrap_or(0)
    }
    fn po(&self, p: u32, o: u32) -> usize {
        self.pos_pair_count.get(&(p, o)).copied().unwrap_or(0)
    }
    fn os(&self, o: u32, s: u32) -> usize {
        self.osp_pair_count.get(&(o, s)).copied().unwrap_or(0)
    }
    fn sdeg(&self, s: u32) -> usize {
        self.subject_degree.get(&s).copied().unwrap_or(0)
    }
    fn pdeg(&self, p: u32) -> usize {
        self.predicate_degree.get(&p).copied().unwrap_or(0)
    }
    fn odeg(&self, o: u32) -> usize {
        self.object_degree.get(&o).copied().unwrap_or(0)
    }
}

/// Precomputed statistics over the whole triple store.
///
/// All cardinalities are exact (no sampling) and O(1) to look up. The pair
/// counts allow a precise estimate even for patterns with two bound terms.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub total_triples: usize,

    pub subject_degree: FxHashMap<u32, usize>,
    pub predicate_degree: FxHashMap<u32, usize>,
    pub object_degree: FxHashMap<u32, usize>,

    pub spo_pair_count: FxHashMap<(u32, u32), usize>, // (S, P) -> #O
    pub pos_pair_count: FxHashMap<(u32, u32), usize>, // (P, O) -> #S
    pub osp_pair_count: FxHashMap<(u32, u32), usize>, // (O, S) -> #P
}

impl Stats {
    pub fn from_triples(triples: &[(u32, u32, u32)]) -> Self {
        let mut stats = Self {
            total_triples: triples.len(),
            ..Default::default()
        };

        for (s, p, o) in triples {
            *stats.subject_degree.entry(*s).or_insert(0) += 1;
            *stats.predicate_degree.entry(*p).or_insert(0) += 1;
            *stats.object_degree.entry(*o).or_insert(0) += 1;

            *stats.spo_pair_count.entry((*s, *p)).or_insert(0) += 1;
            *stats.pos_pair_count.entry((*p, *o)).or_insert(0) += 1;
            *stats.osp_pair_count.entry((*o, *s)).or_insert(0) += 1;
        }

        stats
    }

    /// Incremental update: records a newly added (distinct) triple.
    pub fn add_triple(&mut self, s: u32, p: u32, o: u32) {
        self.total_triples += 1;
        *self.subject_degree.entry(s).or_insert(0) += 1;
        *self.predicate_degree.entry(p).or_insert(0) += 1;
        *self.object_degree.entry(o).or_insert(0) += 1;
        *self.spo_pair_count.entry((s, p)).or_insert(0) += 1;
        *self.pos_pair_count.entry((p, o)).or_insert(0) += 1;
        *self.osp_pair_count.entry((o, s)).or_insert(0) += 1;
    }

    /// Incremental update: records a removed triple. Counters that drop to 0
    /// are removed from the maps.
    pub fn remove_triple(&mut self, s: u32, p: u32, o: u32) {
        self.total_triples = self.total_triples.saturating_sub(1);
        dec(&mut self.subject_degree, s);
        dec(&mut self.predicate_degree, p);
        dec(&mut self.object_degree, o);
        dec(&mut self.spo_pair_count, (s, p));
        dec(&mut self.pos_pair_count, (p, o));
        dec(&mut self.osp_pair_count, (o, s));
    }

    /// Number of entries across all maps (for the memory report).
    pub fn entry_count(&self) -> usize {
        self.subject_degree.len()
            + self.predicate_degree.len()
            + self.object_degree.len()
            + self.spo_pair_count.len()
            + self.pos_pair_count.len()
            + self.osp_pair_count.len()
    }

    /// Rough byte estimate of the statistics maps.
    pub fn approx_bytes(&self) -> usize {
        // Degree maps: u32->usize (~14 B/entry incl. hashbrown overhead).
        let deg =
            (self.subject_degree.len() + self.predicate_degree.len() + self.object_degree.len())
                * 14;
        // Pair maps: (u32,u32)->usize (~20 B/entry).
        let pair =
            (self.spo_pair_count.len() + self.pos_pair_count.len() + self.osp_pair_count.len())
                * 20;
        deg + pair
    }

    /// Estimates the cardinality (number of result rows) of a pattern.
    /// Runs in O(1) thanks to the precomputed hash maps.
    pub fn estimate_cardinality(&self, s: &PatternTerm, p: &PatternTerm, o: &PatternTerm) -> usize {
        use PatternTerm::{Bound, Variable};

        match (s, p, o) {
            // Exact triple
            (Bound(_), Bound(_), Bound(_)) => 1,

            // One wildcard
            (Bound(sv), Bound(pv), Variable(_)) => {
                self.spo_pair_count.get(&(*sv, *pv)).copied().unwrap_or(0)
            }
            (Variable(_), Bound(pv), Bound(ov)) => {
                self.pos_pair_count.get(&(*pv, *ov)).copied().unwrap_or(0)
            }
            (Bound(sv), Variable(_), Bound(ov)) => {
                self.osp_pair_count.get(&(*ov, *sv)).copied().unwrap_or(0)
            }

            // Two wildcards
            (Bound(sv), Variable(_), Variable(_)) => {
                self.subject_degree.get(sv).copied().unwrap_or(0)
            }
            (Variable(_), Bound(pv), Variable(_)) => {
                self.predicate_degree.get(pv).copied().unwrap_or(0)
            }
            (Variable(_), Variable(_), Bound(ov)) => {
                self.object_degree.get(ov).copied().unwrap_or(0)
            }

            // Three wildcards
            (Variable(_), Variable(_), Variable(_)) => self.total_triples,
        }
    }
}

/// Decrements a counter in the map, removing the entry at 0.
fn dec<K: std::hash::Hash + Eq>(map: &mut FxHashMap<K, usize>, key: K) {
    if let Some(v) = map.get_mut(&key) {
        if *v <= 1 {
            map.remove(&key);
        } else {
            *v -= 1;
        }
    }
}
