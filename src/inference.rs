//! SPARQL algebra rewriter for backward-chaining deductive inference.
//!
//! Works **entirely at query time** — the stored index is never modified.
//! Each `Bgp` node in the parsed algebra tree is expanded with `Union`
//! branches that capture triples derivable through RDFS rules.
//!
//! ## Supported rules
//!
//! | Rule | Rewrite |
//! |------|---------|
//! | `rdfs:subClassOf` | `?s rdf:type ?o` → `?s rdf:type ?t . ?t rdfs:subClassOf ?o` |
//! | `rdfs:subPropertyOf` | `?s ?p ?o` → `?s ?q ?o . ?q rdfs:subPropertyOf ?p` |
//! | `rdfs:domain` | `?s rdf:type ?c` → `?s ?p ?o . ?p rdfs:domain ?c` |
//! | `rdfs:range` | `?o rdf:type ?c` → `?s ?p ?o . ?p rdfs:range ?c` |

use spargebra::algebra::GraphPattern as GP;
use spargebra::term::{NamedNode, NamedNodePattern, TermPattern, TriplePattern, Variable};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const RDFS_SUBPROPERTY_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";

/// Rewrite a SPARQL algebra tree to include RDFS backward-chaining inferences.
///
/// Call this **after** parsing but **before** any evaluation. Every `Bgp` node
/// is replaced with a `Union` of the original plus one branch per applicable
/// inference rule.
pub fn rewrite(pattern: GP) -> GP {
    rewrite_gp(pattern, &mut 0)
}

fn rewrite_gp(gp: GP, fresh: &mut u32) -> GP {
    match gp {
        GP::Bgp { patterns } => {
            let branches = inference_branches(&patterns, fresh);
            if branches.is_empty() {
                return GP::Bgp { patterns };
            }
            // Nest as left-deep Union chain: original ∪ branch1 ∪ branch2 …
            let mut result = GP::Bgp { patterns };
            for branch in branches {
                result = GP::Union {
                    left: Box::new(result),
                    right: Box::new(branch),
                };
            }
            result
        }
        GP::Join { left, right } => GP::Join {
            left: Box::new(rewrite_gp(*left, fresh)),
            right: Box::new(rewrite_gp(*right, fresh)),
        },
        GP::LeftJoin {
            left,
            right,
            expression,
        } => GP::LeftJoin {
            left: Box::new(rewrite_gp(*left, fresh)),
            right: Box::new(rewrite_gp(*right, fresh)),
            expression,
        },
        GP::Union { left, right } => GP::Union {
            left: Box::new(rewrite_gp(*left, fresh)),
            right: Box::new(rewrite_gp(*right, fresh)),
        },
        GP::Filter { expr, inner } => GP::Filter {
            expr,
            inner: Box::new(rewrite_gp(*inner, fresh)),
        },
        GP::Extend {
            inner,
            variable,
            expression,
        } => GP::Extend {
            inner: Box::new(rewrite_gp(*inner, fresh)),
            variable,
            expression,
        },
        GP::Minus { left, right } => GP::Minus {
            left: Box::new(rewrite_gp(*left, fresh)),
            right: Box::new(rewrite_gp(*right, fresh)),
        },
        GP::Project { inner, variables } => GP::Project {
            inner: Box::new(rewrite_gp(*inner, fresh)),
            variables,
        },
        GP::Distinct { inner } => GP::Distinct {
            inner: Box::new(rewrite_gp(*inner, fresh)),
        },
        GP::Reduced { inner } => GP::Reduced {
            inner: Box::new(rewrite_gp(*inner, fresh)),
        },
        GP::OrderBy { inner, expression } => GP::OrderBy {
            inner: Box::new(rewrite_gp(*inner, fresh)),
            expression,
        },
        GP::Slice {
            inner,
            start,
            length,
        } => GP::Slice {
            inner: Box::new(rewrite_gp(*inner, fresh)),
            start,
            length,
        },
        GP::Group {
            inner,
            variables,
            aggregates,
        } => GP::Group {
            inner: Box::new(rewrite_gp(*inner, fresh)),
            variables,
            aggregates,
        },
        GP::Path { .. } | GP::Values { .. } | GP::Service { .. } | GP::Graph { .. } => gp,
    }
}

/// Build inference `Bgp` branches for a list of triple patterns.
///
/// For each pattern that triggers a rule, a **complete** copy of the BGP is
/// produced with that one pattern replaced by the inferred variant(s). This
/// guarantees that cross-pattern variable bindings (e.g. `?s` joined across
/// several triples) are preserved in every branch.
fn inference_branches(patterns: &[TriplePattern], fresh: &mut u32) -> Vec<GP> {
    let mut branches = Vec::new();

    for (i, tp) in patterns.iter().enumerate() {
        // ── rdfs:subClassOf ────────────────────────────────────────────
        //   ?s rdf:type ?o  →  ?s rdf:type ?t . ?t rdfs:subClassOf ?o
        if is_type_pattern(tp) {
            let t_var = var(format!("__infer_sc_{}", next_id(fresh)));
            let mut inferred: Vec<TriplePattern> = patterns.to_vec();
            inferred[i] = TriplePattern {
                subject: tp.subject.clone(),
                predicate: type_nn(),
                object: TermPattern::Variable(t_var.clone()),
            };
            inferred.push(TriplePattern {
                subject: TermPattern::Variable(t_var),
                predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(RDFS_SUBCLASS_OF)),
                object: tp.object.clone(),
            });
            branches.push(GP::Bgp { patterns: inferred });
        }

        // ── rdfs:subPropertyOf ─────────────────────────────────────────
        //   ?s ?p ?o  →  ?s ?q ?o . ?q rdfs:subPropertyOf ?p
        //   ?s ex:p ?o →  ?s ?q ?o . ?q rdfs:subPropertyOf ex:p
        if !is_type_predicate(&tp.predicate) {
            let q_var = var(format!("__infer_sp_{}", next_id(fresh)));
            let mut inferred: Vec<TriplePattern> = patterns.to_vec();
            inferred[i] = TriplePattern {
                subject: tp.subject.clone(),
                predicate: NamedNodePattern::Variable(q_var.clone()),
                object: tp.object.clone(),
            };
            let sub_prop_obj = match &tp.predicate {
                NamedNodePattern::NamedNode(nn) => TermPattern::NamedNode(nn.clone()),
                NamedNodePattern::Variable(v) => TermPattern::Variable(v.clone()),
            };
            inferred.push(TriplePattern {
                subject: TermPattern::Variable(q_var),
                predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(
                    RDFS_SUBPROPERTY_OF,
                )),
                object: sub_prop_obj,
            });
            branches.push(GP::Bgp { patterns: inferred });
        }

        // ── rdfs:domain ────────────────────────────────────────────────
        //   ?s rdf:type ?c  →  ?s ?p ?o . ?p rdfs:domain ?c
        if is_type_pattern(tp) {
            let p_var = var(format!("__infer_dom_p_{}", next_id(fresh)));
            let o_var = var(format!("__infer_dom_o_{}", next_id(fresh)));
            let mut inferred: Vec<TriplePattern> = patterns.to_vec();
            inferred[i] = TriplePattern {
                subject: tp.subject.clone(),
                predicate: NamedNodePattern::Variable(p_var.clone()),
                object: TermPattern::Variable(o_var),
            };
            inferred.push(TriplePattern {
                subject: TermPattern::Variable(p_var),
                predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(RDFS_DOMAIN)),
                object: tp.object.clone(),
            });
            branches.push(GP::Bgp { patterns: inferred });
        }

        // ── rdfs:range ─────────────────────────────────────────────────
        //   ?o rdf:type ?c  →  ?s ?p ?o . ?p rdfs:range ?c
        if is_type_pattern(tp) {
            let s_var = var(format!("__infer_range_s_{}", next_id(fresh)));
            let p_var = var(format!("__infer_range_p_{}", next_id(fresh)));
            let mut inferred: Vec<TriplePattern> = patterns.to_vec();
            inferred[i] = TriplePattern {
                subject: TermPattern::Variable(s_var),
                predicate: NamedNodePattern::Variable(p_var.clone()),
                object: tp.object.clone(),
            };
            inferred.push(TriplePattern {
                subject: TermPattern::Variable(p_var),
                predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(RDFS_RANGE)),
                object: tp.object.clone(),
            });
            branches.push(GP::Bgp { patterns: inferred });
        }
    }

    branches
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn next_id(counter: &mut u32) -> u32 {
    let n = *counter;
    *counter += 1;
    n
}

fn var(name: String) -> Variable {
    Variable::new_unchecked(name)
}

fn type_nn() -> NamedNodePattern {
    NamedNodePattern::NamedNode(NamedNode::new_unchecked(RDF_TYPE))
}

fn is_type_pattern(tp: &TriplePattern) -> bool {
    is_type_predicate(&tp.predicate)
}

fn is_type_predicate(p: &NamedNodePattern) -> bool {
    matches!(p, NamedNodePattern::NamedNode(nn) if nn.as_str() == RDF_TYPE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spargebra::SparqlParser;

    fn rewrite_query(query_str: &str) -> String {
        let mut query = SparqlParser::new().parse_query(query_str).unwrap();
        match &mut query {
            spargebra::Query::Select { pattern, .. } => {
                *pattern = rewrite(std::mem::replace(pattern, GP::Bgp { patterns: vec![] }));
            }
            spargebra::Query::Ask { pattern, .. } => {
                *pattern = rewrite(std::mem::replace(pattern, GP::Bgp { patterns: vec![] }));
            }
            _ => {}
        }
        query.to_string()
    }

    #[test]
    fn subclasof_rule_expands_type_pattern() {
        let rewritten = rewrite_query(
            "SELECT ?s WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Person> }",
        );
        assert!(
            rewritten.contains("UNION"),
            "expected UNION in rewritten query, got: {rewritten}"
        );
        // Should mention rdfs:subClassOf somewhere.
        assert!(
            rewritten.contains("http://www.w3.org/2000/01/rdf-schema#subClassOf"),
            "expected subClassOf reference, got: {rewritten}"
        );
    }

    #[test]
    fn subproperty_rewrites_non_type_pattern() {
        // Even without rdf:type, the subPropertyOf rule applies.
        let original = "SELECT ?s WHERE { ?s <http://example.org/knows> ?o }";
        let rewritten = rewrite_query(original);
        assert!(
            rewritten.contains("subPropertyOf"),
            "expected subPropertyOf rewrite, got: {rewritten}"
        );
    }

    #[test]
    fn subproperty_rule_expands() {
        let rewritten = rewrite_query("SELECT ?s WHERE { ?s <http://example.org/name> ?o }");
        assert!(
            rewritten.contains("subPropertyOf"),
            "expected subPropertyOf reference, got: {rewritten}"
        );
    }

    #[test]
    fn domain_rule_expands() {
        let rewritten = rewrite_query(
            "SELECT ?s WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Person> }",
        );
        assert!(
            rewritten.contains("http://www.w3.org/2000/01/rdf-schema#domain"),
            "expected domain reference, got: {rewritten}"
        );
    }

    #[test]
    fn range_rule_expands() {
        let rewritten = rewrite_query(
            "SELECT ?s WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Person> }",
        );
        assert!(
            rewritten.contains("http://www.w3.org/2000/01/rdf-schema#range"),
            "expected range reference, got: {rewritten}"
        );
    }

    #[test]
    fn preserves_non_type_patterns() {
        // A BGP with both rdf:type and another pattern keeps both.
        let rewritten = rewrite_query(
            "SELECT ?s ?name WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Person> . ?s <http://example.org/name> ?name }",
        );
        // The original patterns should still be in the output.
        assert!(
            rewritten.contains("http://example.org/name"),
            "expected original pattern to survive, got: {rewritten}"
        );
    }
}
