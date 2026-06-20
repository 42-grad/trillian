use rustc_hash::{FxHashMap, FxHashSet};

use super::executor::{execute_plan, execute_wcoj};
use super::planner::GraphPattern;
use super::query::TripleStore;

/// Hybride Abfrage-Engine.
///
/// Analysiert das `GraphPattern` auf Zyklen im Variablen-Abhängigkeitsgraphen
/// und wählt dann automatisch den besten Ausführungspfad:
///
/// * **Zyklus vorhanden** → `execute_wcoj` (Worst-Case-Optimal Join).
/// * **Azyklisch** (Stern, Pfad, Baum) → kostenbasierter Binär-Planner
///   (`execute_plan` mit `GraphPattern::optimize`).
///
/// Dies spiegelt die Strategie des originalen Tentris wider: lineare Joins
/// über den kostenbasierten Planer, zyklische Muster über WCOJ/Einsum.
#[derive(Debug, Clone, Default)]
pub struct HybridEngine;

impl HybridEngine {
    pub fn new() -> Self {
        Self
    }

    /// Führt ein Graph-Pattern aus und wählt automatisch den optimalen Pfad.
    pub fn execute(&self, store: &TripleStore, pattern: &GraphPattern) -> Vec<Vec<u32>> {
        if has_cycle(pattern) {
            execute_wcoj(store, pattern)
        } else {
            let plan = pattern.optimize(&store.stats);
            execute_plan(store, pattern, &plan)
        }
    }
}

/// Baut den Variablen-Abhängigkeitsgraphen auf und prüft auf Zyklen.
///
/// Jede Kante verbindet zwei Variablen, die im selben Triple-Muster
/// vorkommen. Ein Zyklus in diesem Graphen bedeutet, dass das Pattern
/// zyklisch ist (z. B. Dreieck).
fn has_cycle(pattern: &GraphPattern) -> bool {
    if pattern.patterns.len() < 2 {
        return false;
    }

    let mut adj: FxHashMap<String, Vec<String>> = FxHashMap::default();

    for pat in &pattern.patterns {
        let vars = pat.variables();
        // Clique zwischen allen Variablen im Muster
        for (i, v1) in vars.iter().enumerate() {
            for v2 in vars.iter().skip(i + 1) {
                adj.entry((*v1).clone())
                    .or_default()
                    .push((*v2).clone());
                adj.entry((*v2).clone())
                    .or_default()
                    .push((*v1).clone());
            }
        }
    }

    let mut visited: FxHashSet<String> = FxHashSet::default();

    for start in adj.keys() {
        if !visited.contains(start) {
            if dfs_cycle(start, None, &adj, &mut visited) {
                return true;
            }
        }
    }

    false
}

fn dfs_cycle(
    node: &str,
    parent: Option<&str>,
    adj: &FxHashMap<String, Vec<String>>,
    visited: &mut FxHashSet<String>,
) -> bool {
    visited.insert(node.to_string());

    if let Some(neighbors) = adj.get(node) {
        for neighbor in neighbors {
            if Some(neighbor.as_str()) == parent {
                continue;
            }
            if visited.contains(neighbor) {
                return true;
            }
            if dfs_cycle(neighbor, Some(node), adj, visited) {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypertrie::planner::{PatternTerm, TriplePattern};

    fn cycle_pattern() -> GraphPattern {
        GraphPattern {
            patterns: vec![
                TriplePattern {
                    subject: PatternTerm::Variable("a".to_string()),
                    predicate: PatternTerm::Bound(1),
                    object: PatternTerm::Variable("b".to_string()),
                },
                TriplePattern {
                    subject: PatternTerm::Variable("b".to_string()),
                    predicate: PatternTerm::Bound(1),
                    object: PatternTerm::Variable("c".to_string()),
                },
                TriplePattern {
                    subject: PatternTerm::Variable("c".to_string()),
                    predicate: PatternTerm::Bound(1),
                    object: PatternTerm::Variable("a".to_string()),
                },
            ],
        }
    }

    fn path_pattern() -> GraphPattern {
        GraphPattern {
            patterns: vec![
                TriplePattern {
                    subject: PatternTerm::Variable("a".to_string()),
                    predicate: PatternTerm::Bound(1),
                    object: PatternTerm::Variable("b".to_string()),
                },
                TriplePattern {
                    subject: PatternTerm::Variable("b".to_string()),
                    predicate: PatternTerm::Bound(1),
                    object: PatternTerm::Variable("c".to_string()),
                },
            ],
        }
    }

    #[test]
    fn detects_triangle() {
        assert!(has_cycle(&cycle_pattern()));
    }

    #[test]
    fn path_is_acyclic() {
        assert!(!has_cycle(&path_pattern()));
    }

    #[test]
    fn star_is_acyclic() {
        let star = GraphPattern {
            patterns: vec![
                TriplePattern {
                    subject: PatternTerm::Variable("x".to_string()),
                    predicate: PatternTerm::Bound(1),
                    object: PatternTerm::Variable("y1".to_string()),
                },
                TriplePattern {
                    subject: PatternTerm::Variable("x".to_string()),
                    predicate: PatternTerm::Bound(1),
                    object: PatternTerm::Variable("y2".to_string()),
                },
            ],
        };
        assert!(!has_cycle(&star));
    }
}
