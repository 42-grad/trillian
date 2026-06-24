#[cfg(test)]
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

#[cfg(test)]
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

/// Precomputed, exact statistics over a triple set — a simple [`CardEstimator`]
/// used by the planner tests to drive deterministic cardinalities. The engine
/// itself estimates on demand from the indexes (`impl CardEstimator for
/// TripleStore`), so this type is test-only.
#[cfg(test)]
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
