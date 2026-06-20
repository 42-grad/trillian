use rustc_hash::FxHashMap;

use super::planner::PatternTerm;

/// Statistische Metadaten über den gesamten Triple-Store.
///
/// Alle Kardinalitäten sind exakt (keine Stichproben) und in O(1)
/// abrufbar. Die Paar-Counts erlauben eine präzise Schätzung selbst
/// für Muster mit zwei gebundenen Termen.
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

    /// Inkrementelles Update: erfasst ein neu hinzugefügtes (distinktes) Triple.
    pub fn add_triple(&mut self, s: u32, p: u32, o: u32) {
        self.total_triples += 1;
        *self.subject_degree.entry(s).or_insert(0) += 1;
        *self.predicate_degree.entry(p).or_insert(0) += 1;
        *self.object_degree.entry(o).or_insert(0) += 1;
        *self.spo_pair_count.entry((s, p)).or_insert(0) += 1;
        *self.pos_pair_count.entry((p, o)).or_insert(0) += 1;
        *self.osp_pair_count.entry((o, s)).or_insert(0) += 1;
    }

    /// Inkrementelles Update: erfasst ein entferntes Triple. Zähler, die auf
    /// 0 fallen, werden aus den Maps entfernt.
    pub fn remove_triple(&mut self, s: u32, p: u32, o: u32) {
        self.total_triples = self.total_triples.saturating_sub(1);
        dec(&mut self.subject_degree, s);
        dec(&mut self.predicate_degree, p);
        dec(&mut self.object_degree, o);
        dec(&mut self.spo_pair_count, (s, p));
        dec(&mut self.pos_pair_count, (p, o));
        dec(&mut self.osp_pair_count, (o, s));
    }

    /// Schätzt die Kardinalität (Anzahl Ergebnisreihen) eines Musters.
    /// Läuft in O(1) dank vorberechneter Hash-Maps.
    pub fn estimate_cardinality(
        &self,
        s: &PatternTerm,
        p: &PatternTerm,
        o: &PatternTerm,
    ) -> usize {
        use PatternTerm::{Bound, Variable};

        match (s, p, o) {
            // Exaktes Triple
            (Bound(_), Bound(_), Bound(_)) => 1,

            // Ein Wildcard
            (Bound(sv), Bound(pv), Variable(_)) => {
                self.spo_pair_count.get(&(*sv, *pv)).copied().unwrap_or(0)
            }
            (Variable(_), Bound(pv), Bound(ov)) => {
                self.pos_pair_count.get(&(*pv, *ov)).copied().unwrap_or(0)
            }
            (Bound(sv), Variable(_), Bound(ov)) => {
                self.osp_pair_count.get(&(*ov, *sv)).copied().unwrap_or(0)
            }

            // Zwei Wildcards
            (Bound(sv), Variable(_), Variable(_)) => {
                self.subject_degree.get(sv).copied().unwrap_or(0)
            }
            (Variable(_), Bound(pv), Variable(_)) => {
                self.predicate_degree.get(pv).copied().unwrap_or(0)
            }
            (Variable(_), Variable(_), Bound(ov)) => {
                self.object_degree.get(ov).copied().unwrap_or(0)
            }

            // Drei Wildcards
            (Variable(_), Variable(_), Variable(_)) => self.total_triples,
        }
    }
}

/// Dekrementiert einen Zähler in der Map und entfernt den Eintrag bei 0.
fn dec<K: std::hash::Hash + Eq>(map: &mut FxHashMap<K, usize>, key: K) {
    if let Some(v) = map.get_mut(&key) {
        if *v <= 1 {
            map.remove(&key);
        } else {
            *v -= 1;
        }
    }
}
