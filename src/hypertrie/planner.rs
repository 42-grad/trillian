use super::stats::{estimate_cardinality, CardEstimator};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PatternTerm {
    Bound(u32),
    Variable(String),
}

#[derive(Debug, Clone)]
pub struct TriplePattern {
    pub subject: PatternTerm,
    pub predicate: PatternTerm,
    pub object: PatternTerm,
}

impl TriplePattern {
    pub fn variables(&self) -> Vec<&String> {
        let mut vars = Vec::new();
        for term in [&self.subject, &self.predicate, &self.object] {
            if let PatternTerm::Variable(name) = term {
                vars.push(name);
            }
        }
        vars
    }
}

#[derive(Debug, Clone)]
pub struct GraphPattern {
    pub patterns: Vec<TriplePattern>,
}

impl GraphPattern {
    /// Liefert alle Variablen in ihrer ersten Erscheinungsreihenfolge.
    /// Das ist dieselbe Reihenfolge, in der `execute_plan` und `execute_wcoj`
    /// die Ergebniszeilen materialisieren.
    pub fn variable_order(&self) -> Vec<&String> {
        let mut seen = rustc_hash::FxHashSet::default();
        let mut order = Vec::new();
        for pat in &self.patterns {
            for var in pat.variables() {
                if seen.insert(var) {
                    order.push(var);
                }
            }
        }
        order
    }
}

#[derive(Debug, Clone)]
pub struct PlanStep {
    pub pattern_index: usize,
}

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub steps: Vec<PlanStep>,
}

impl ExecutionPlan {
    /// Naive Plan: führt die Muster in der Reihenfolge der Eingabe aus.
    pub fn naive(n: usize) -> Self {
        Self {
            steps: (0..n).map(|i| PlanStep { pattern_index: i }).collect(),
        }
    }
}

impl GraphPattern {
    /// Kostenbasierter, gieriger Optimierer.
    ///
    /// 1. Starte mit dem selektivsten Muster (niedrigste Kardinalität).
    /// 2. Füge immer das noch nicht geplante Muster hinzu, das
    ///    a) mindestens eine Variable mit dem bereits geplanten Teil teilt und
    ///    b) unter diesen die niedrigste Kardinalität hat.
    /// 3. Falls keine Verbindung besteht (z. B. Kreuzprodukt), nimm das
    ///    selektivste verbleibende Muster.
    pub fn optimize<E: CardEstimator + ?Sized>(&self, est: &E) -> ExecutionPlan {
        let n = self.patterns.len();
        if n == 0 {
            return ExecutionPlan { steps: Vec::new() };
        }

        let mut planned = vec![false; n];
        let mut steps = Vec::with_capacity(n);

        // Schritt 1: selektivstes Muster als Startpunkt
        let mut best_idx = 0;
        let mut best_cost = usize::MAX;
        for (i, pat) in self.patterns.iter().enumerate() {
            let cost = estimate_cardinality(est, &pat.subject, &pat.predicate, &pat.object);
            if cost < best_cost {
                best_cost = cost;
                best_idx = i;
            }
        }
        planned[best_idx] = true;
        steps.push(PlanStep {
            pattern_index: best_idx,
        });

        // Schritt 2+ : gierig verbundene Muster hinzufügen
        while steps.len() < n {
            let mut pick = None;
            let mut pick_cost = usize::MAX;

            for i in 0..n {
                if planned[i] {
                    continue;
                }

                let connected = steps.iter().any(|step| {
                    self.patterns_share_variables(i, step.pattern_index)
                });

                if !connected {
                    continue;
                }

                let pat = &self.patterns[i];
                let cost =
                    estimate_cardinality(est, &pat.subject, &pat.predicate, &pat.object);
                if cost < pick_cost {
                    pick_cost = cost;
                    pick = Some(i);
                }
            }

            // Fallback: keine Verbindung -> selektivstes verbleibendes Muster
            let idx = pick.unwrap_or_else(|| {
                let mut best = 0;
                let mut best_cost = usize::MAX;
                for i in 0..n {
                    if planned[i] {
                        continue;
                    }
                    let pat = &self.patterns[i];
                    let cost =
                        estimate_cardinality(est, &pat.subject, &pat.predicate, &pat.object);
                    if cost < best_cost {
                        best_cost = cost;
                        best = i;
                    }
                }
                best
            });

            planned[idx] = true;
            steps.push(PlanStep { pattern_index: idx });
        }

        ExecutionPlan { steps }
    }

    fn patterns_share_variables(&self, i: usize, j: usize) -> bool {
        let vars_i: std::collections::HashSet<_> =
            self.patterns[i].variables().into_iter().collect();
        self.patterns[j]
            .variables()
            .into_iter()
            .any(|v| vars_i.contains(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypertrie::stats::Stats;

    fn make_pattern() -> GraphPattern {
        GraphPattern {
            patterns: vec![
                // (?x, p1, ?y) - cardinality 100
                TriplePattern {
                    subject: PatternTerm::Variable("x".to_string()),
                    predicate: PatternTerm::Bound(1),
                    object: PatternTerm::Variable("y".to_string()),
                },
                // (?y, p2, const) - cardinality 5
                TriplePattern {
                    subject: PatternTerm::Variable("y".to_string()),
                    predicate: PatternTerm::Bound(2),
                    object: PatternTerm::Bound(99),
                },
                // (?x, p3, ?z) - cardinality 50
                TriplePattern {
                    subject: PatternTerm::Variable("x".to_string()),
                    predicate: PatternTerm::Bound(3),
                    object: PatternTerm::Variable("z".to_string()),
                },
            ],
        }
    }

    #[test]
    fn optimizer_picks_most_selective_first() {
        let mut stats = Stats::default();
        stats.total_triples = 155;
        stats.predicate_degree.insert(1, 100);
        stats.predicate_degree.insert(2, 5);
        stats.predicate_degree.insert(3, 50);

        let pattern = make_pattern();
        let plan = pattern.optimize(&stats);

        assert_eq!(plan.steps[0].pattern_index, 1); // cardinality 5
    }

    #[test]
    fn optimizer_prefers_connected_patterns() {
        let mut stats = Stats::default();
        stats.total_triples = 155;
        stats.predicate_degree.insert(1, 100);
        stats.predicate_degree.insert(2, 5);
        stats.predicate_degree.insert(3, 50);

        let pattern = make_pattern();
        let plan = pattern.optimize(&stats);

        // Nach Pattern 1 (y) muss Pattern 0 folgen, weil es y teilt.
        assert_eq!(plan.steps[1].pattern_index, 0);
        // Pattern 2 teilt x mit Pattern 0.
        assert_eq!(plan.steps[2].pattern_index, 2);
    }
}
