use rustc_hash::{FxHashMap, FxHashSet};

use super::executor::{RowBlock, execute_plan_limited, execute_wcoj_limited};
use super::planner::GraphPattern;
use super::query::TripleStore;

/// Hybrid query engine.
///
/// Analyzes the `GraphPattern` for cycles in the variable dependency graph and
/// then automatically picks the best execution path:
///
/// * **Cycle present** → `execute_wcoj` (worst-case-optimal join).
/// * **Acyclic** (star, path, tree) → cost-based binary planner
///   (`execute_plan` with `GraphPattern::optimize`).
///
/// This mirrors common practice for worst-case-optimal join engines: linear joins
/// via the cost-based planner, cyclic patterns via WCOJ (leapfrog triejoin).
#[derive(Debug, Clone, Default)]
pub struct HybridEngine;

impl HybridEngine {
    pub fn new() -> Self {
        Self
    }

    /// Executes a graph pattern, automatically picking the optimal path.
    /// Returns `Err` if the result set exceeds the row cap (guards against OOM
    /// on degenerate queries), see `executor::max_result_rows`.
    pub fn execute(&self, store: &TripleStore, pattern: &GraphPattern) -> Result<RowBlock, String> {
        self.execute_limited(store, pattern, None)
    }

    /// Like [`execute`](Self::execute), but terminates early once `limit` final
    /// rows have been produced (LIMIT pushdown — bounds memory + time; only valid
    /// when there is no ORDER BY/DISTINCT on top).
    pub fn execute_limited(
        &self,
        store: &TripleStore,
        pattern: &GraphPattern,
        limit: Option<usize>,
    ) -> Result<RowBlock, String> {
        if has_cycle(pattern) {
            execute_wcoj_limited(store, pattern, limit)
        } else {
            let plan = pattern.optimize(store);
            execute_plan_limited(store, pattern, &plan, limit)
        }
    }
}

/// Builds the variable dependency graph and checks for cycles.
///
/// Each edge connects two variables that occur in the same triple pattern.
/// A cycle in this graph means the pattern is cyclic (e.g. a triangle).
fn has_cycle(pattern: &GraphPattern) -> bool {
    if pattern.patterns.len() < 2 {
        return false;
    }

    let mut adj: FxHashMap<String, Vec<String>> = FxHashMap::default();

    for pat in &pattern.patterns {
        let vars = pat.variables();
        // Clique between all variables in the pattern
        for (i, v1) in vars.iter().enumerate() {
            for v2 in vars.iter().skip(i + 1) {
                adj.entry((*v1).clone()).or_default().push((*v2).clone());
                adj.entry((*v2).clone()).or_default().push((*v1).clone());
            }
        }
    }

    let mut visited: FxHashSet<String> = FxHashSet::default();

    for start in adj.keys() {
        if !visited.contains(start) && dfs_cycle(start, None, &adj, &mut visited) {
            return true;
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
