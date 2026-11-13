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
//! | rdfs9 + rdfs11 | `?s rdf:type ?c` → `?s rdf:type ?t . ?t rdfs:subClassOf+ ?c` |
//! | rdfs2 + rdfs5 + rdfs11 | `?s rdf:type ?c` → `?s ?p ?o . ?p rdfs:subPropertyOf*/rdfs:domain/rdfs:subClassOf* ?c` |
//! | rdfs3 + rdfs5 + rdfs11 | `?s rdf:type ?c` → `?x ?p ?s . ?p rdfs:subPropertyOf*/rdfs:range/rdfs:subClassOf* ?c` |
//! | rdfs7 + rdfs5 | `?s ?p ?o` → `?s ?q ?o . ?q rdfs:subPropertyOf+ ?p` |
//! | rdfs9 + rdfs5 | `?s rdf:type ?c` → `?s ?q ?t . ?q rdfs:subPropertyOf+ rdf:type . ?t rdfs:subClassOf* ?c` |
//!
//! Two things follow from compiling each rule's schema lookup into a single
//! **property path**. The transitive rules are closed by the path evaluator, so
//! the rewrite itself needs no fixpoint iteration and no depth bound. And the
//! rules compose inside one path — `rdfs:domain`/`rdfs:range` fire through a
//! `rdfs:subPropertyOf` chain on the predicate and yield a class that is then
//! generalized along `rdfs:subClassOf` — which closes the rule set at three
//! branches per `rdf:type` pattern instead of a branch per rule combination.
//!
//! The path is the **left** side of each branch's join, and a class the schema
//! never reaches makes it empty, so a rule that cannot fire costs one closure
//! walk from a bound term rather than a scan of the data.
//!
//! Every branch is an independent witness of the same entailed triple, so a
//! rewritten `Bgp` is wrapped in `Project`/`Distinct` over the columns the
//! pattern itself binds: the branches' helper variables never leave the node,
//! and one entailed triple yields one row.
//!
//! Property paths are expanded into the equivalent triple patterns when they
//! lead with plain predicates, so the `rdf:type` step inside
//! `?s rdf:type/rdfs:subClassOf* ?c` carries the type branches instead of
//! looking at asserted triples only.
//!
//! Two gaps remain: each branch rewrites one pattern, so a solution that needs
//! inference at two patterns of the same BGP is missed; and the
//! `rdfs:domain`/`rdfs:range` branches read every triple of an unbound entity,
//! because the schema path binds the predicate but nothing pushes that binding
//! into the BGP.

use spargebra::algebra::{GraphPattern as GP, PropertyPathExpression as Ppe};
use spargebra::term::{NamedNode, NamedNodePattern, TermPattern, TriplePattern, Variable};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_SUBCLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const RDFS_SUBPROPERTY_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";

/// Rewrite a SPARQL algebra tree to include RDFS backward-chaining inferences.
///
/// Call this **after** parsing but **before** any evaluation.
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
            let variables = projected_variables(&patterns);
            // Left-deep Union chain: original ∪ branch1 ∪ branch2 …
            let mut union = GP::Bgp { patterns };
            for branch in branches {
                union = GP::Union {
                    left: Box::new(union),
                    right: Box::new(branch.into_pattern()),
                };
            }
            // Collapse the branches back to one row per entailed triple.
            GP::Distinct {
                inner: Box::new(GP::Project {
                    inner: Box::new(union),
                    variables,
                }),
            }
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
        GP::Path {
            subject,
            path,
            object,
        } => rewrite_path(subject, path, object, fresh),
        GP::Values { .. } | GP::Service { .. } | GP::Graph { .. } => gp,
    }
}

/// Expands a property path into the equivalent triple patterns where it leads
/// with plain predicates, so the BGP branches see the `rdf:type` step inside
/// `?s rdf:type/rdfs:subClassOf* ?c` — without this the whole path looked at
/// asserted triples only. A path that starts with a non-trivial operator
/// (`|`, `*`, `^`, …) is left to the path evaluator.
fn rewrite_path(subject: TermPattern, path: Ppe, object: TermPattern, fresh: &mut u32) -> GP {
    let (steps, tail) = flatten_path(path);
    if steps.is_empty() {
        return match tail {
            Some(path) => GP::Path {
                subject,
                path,
                object,
            },
            None => GP::Bgp { patterns: vec![] },
        };
    }
    let mut patterns = Vec::with_capacity(steps.len());
    let mut current = subject;
    let (last_step, mids) = steps.split_last().expect("steps is non-empty");
    for step in mids {
        let mid = var_term(&fresh_var("path", fresh));
        patterns.push(TriplePattern {
            subject: current,
            predicate: NamedNodePattern::NamedNode(step.clone()),
            object: mid.clone(),
        });
        current = mid;
    }
    // The tail path (if any) hangs off the last step's intermediate node.
    let end = if tail.is_some() {
        var_term(&fresh_var("path", fresh))
    } else {
        object.clone()
    };
    patterns.push(TriplePattern {
        subject: current,
        predicate: NamedNodePattern::NamedNode(last_step.clone()),
        object: end.clone(),
    });
    let bgp = rewrite_gp(GP::Bgp { patterns }, fresh);
    match tail {
        Some(path) => GP::Join {
            // Path first: an empty schema closure short-circuits the data side.
            left: Box::new(GP::Path {
                subject: end,
                path,
                object,
            }),
            right: Box::new(bgp),
        },
        None => bgp,
    }
}

/// Splits a path into leading plain-predicate steps plus the remaining
/// operator tail (`a/b*` → `[a]`, `b*`; `a/(b|c)` → `[a]`, `b|c`).
fn flatten_path(path: Ppe) -> (Vec<NamedNode>, Option<Ppe>) {
    match path {
        Ppe::Sequence(left, right) => {
            let (mut steps, tail) = flatten_path(*left);
            match tail {
                None => {
                    let (more, rest) = flatten_path(*right);
                    steps.extend(more);
                    (steps, rest)
                }
                Some(tail) => (steps, Some(Ppe::Sequence(Box::new(tail), right))),
            }
        }
        Ppe::NamedNode(nn) => (vec![nn], None),
        other => (Vec::new(), Some(other)),
    }
}
/// One inference branch: the rule's schema path, plus a full copy of the BGP
/// with the triggering pattern replaced by the one the rule reads instead.
///
/// The copy is what preserves cross-pattern variable bindings — a branch is a
/// complete alternative solution to the whole BGP, not to one triple.
struct Branch {
    path: (TermPattern, Ppe, TermPattern),
    /// A second schema path, for rules that need two lookups (rdfs9 through a
    /// predicate that is a sub-property of `rdf:type`: the predicate closure
    /// and then the class closure).
    second: Option<(TermPattern, Ppe, TermPattern)>,
    patterns: Vec<TriplePattern>,
}

impl Branch {
    fn new(patterns: &[TriplePattern], path: (TermPattern, Ppe, TermPattern)) -> Self {
        Self {
            path,
            second: None,
            patterns: patterns.to_vec(),
        }
    }

    /// The paths go on the **left**, so an empty schema lookup short-circuits
    /// the join before the data patterns are touched.
    fn into_pattern(self) -> GP {
        let (subject, path, object) = self.path;
        let data = GP::Bgp {
            patterns: self.patterns,
        };
        let right = match self.second {
            Some((subject, path, object)) => GP::Join {
                left: Box::new(GP::Path {
                    subject,
                    path,
                    object,
                }),
                right: Box::new(data),
            },
            None => data,
        };
        GP::Join {
            left: Box::new(GP::Path {
                subject,
                path,
                object,
            }),
            right: Box::new(right),
        }
    }
}

/// The two rules that read a class off a predicate declaration.
#[derive(Clone, Copy)]
enum Declaration {
    /// rdfs2: the entity is the subject of the declaring triple.
    Domain,
    /// rdfs3: the entity is its object.
    Range,
}

fn inference_branches(patterns: &[TriplePattern], fresh: &mut u32) -> Vec<Branch> {
    let mut branches = Vec::new();
    for i in 0..patterns.len() {
        if is_type_predicate(&patterns[i].predicate) {
            branches.push(super_class_branch(patterns, i, fresh));
            branches.push(declaration_branch(patterns, i, Declaration::Domain, fresh));
            branches.push(declaration_branch(patterns, i, Declaration::Range, fresh));
            branches.push(sub_property_type_branch(patterns, i, fresh));
        } else {
            branches.push(sub_property_branch(patterns, i, fresh));
        }
    }
    branches
}

/// `?s rdf:type ?c` ← `?s ?q ?t . ?q rdfs:subPropertyOf+ rdf:type .
/// ?t rdfs:subClassOf* ?c` — rdfs9 through a predicate declared a
/// sub-property of `rdf:type` (e.g. `ex:isA`), then the class hierarchy.
fn sub_property_type_branch(patterns: &[TriplePattern], i: usize, fresh: &mut u32) -> Branch {
    let sub_property = fresh_var("typeprop", fresh);
    let derived = fresh_var("class", fresh);
    let mut branch = Branch::new(
        patterns,
        (
            var_term(&sub_property),
            one_or_more(RDFS_SUBPROPERTY_OF),
            TermPattern::NamedNode(NamedNode::new_unchecked(RDF_TYPE)),
        ),
    );
    branch.second = Some((
        var_term(&derived),
        zero_or_more(RDFS_SUBCLASS_OF),
        patterns[i].object.clone(),
    ));
    branch.patterns[i] = TriplePattern {
        subject: patterns[i].subject.clone(),
        predicate: NamedNodePattern::Variable(sub_property),
        object: var_term(&derived),
    };
    branch
}

/// `?s rdf:type ?c` ← `?s rdf:type ?t . ?t rdfs:subClassOf+ ?c`.
fn super_class_branch(patterns: &[TriplePattern], i: usize, fresh: &mut u32) -> Branch {
    let sub_class = fresh_var("class", fresh);
    let mut branch = Branch::new(
        patterns,
        (
            var_term(&sub_class),
            one_or_more(RDFS_SUBCLASS_OF),
            patterns[i].object.clone(),
        ),
    );
    branch.patterns[i] = TriplePattern {
        subject: patterns[i].subject.clone(),
        predicate: named(RDF_TYPE),
        object: var_term(&sub_class),
    };
    branch
}

/// `?s rdf:type ?c` ← the entity takes part in a triple whose predicate
/// declares `?c` (or a subclass of it) as its domain/range, either itself or
/// through a super-property.
fn declaration_branch(
    patterns: &[TriplePattern],
    i: usize,
    rule: Declaration,
    fresh: &mut u32,
) -> Branch {
    let entity = patterns[i].subject.clone();
    let predicate = fresh_var("pred", fresh);
    let other = var_term(&fresh_var("node", fresh));
    let declaration = match rule {
        Declaration::Domain => RDFS_DOMAIN,
        Declaration::Range => RDFS_RANGE,
    };
    let mut branch = Branch::new(
        patterns,
        (
            var_term(&predicate),
            declaration_path(declaration),
            patterns[i].object.clone(),
        ),
    );
    let predicate = NamedNodePattern::Variable(predicate);
    branch.patterns[i] = match rule {
        Declaration::Domain => TriplePattern {
            subject: entity,
            predicate,
            object: other,
        },
        Declaration::Range => TriplePattern {
            subject: other,
            predicate,
            object: entity,
        },
    };
    branch
}

/// `?s ?p ?o` ← `?s ?q ?o . ?q rdfs:subPropertyOf+ ?p`.
fn sub_property_branch(patterns: &[TriplePattern], i: usize, fresh: &mut u32) -> Branch {
    let sub_property = fresh_var("prop", fresh);
    let queried = match &patterns[i].predicate {
        NamedNodePattern::NamedNode(nn) => TermPattern::NamedNode(nn.clone()),
        NamedNodePattern::Variable(v) => TermPattern::Variable(v.clone()),
    };
    let mut branch = Branch::new(
        patterns,
        (
            var_term(&sub_property),
            one_or_more(RDFS_SUBPROPERTY_OF),
            queried,
        ),
    );
    branch.patterns[i] = TriplePattern {
        subject: patterns[i].subject.clone(),
        predicate: NamedNodePattern::Variable(sub_property),
        object: patterns[i].object.clone(),
    };
    branch
}

/// The columns a rewritten BGP keeps: its own variables plus its blank-node
/// placeholders, in the order plain evaluation binds them.
fn projected_variables(patterns: &[TriplePattern]) -> Vec<Variable> {
    let mut out: Vec<Variable> = Vec::new();
    let add = |v: Variable, out: &mut Vec<Variable>| {
        if !out.contains(&v) {
            out.push(v);
        }
    };
    let term_var = |t: &TermPattern| match t {
        TermPattern::Variable(v) => Some(v.clone()),
        TermPattern::BlankNode(bn) => {
            Some(Variable::new_unchecked(format!("__bn_{}", bn.as_str())))
        }
        _ => None,
    };
    for tp in patterns {
        if let Some(v) = term_var(&tp.subject) {
            add(v, &mut out);
        }
        if let NamedNodePattern::Variable(v) = &tp.predicate {
            add(v.clone(), &mut out);
        }
        if let Some(v) = term_var(&tp.object) {
            add(v, &mut out);
        }
    }
    out
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn fresh_var(kind: &str, counter: &mut u32) -> Variable {
    let n = *counter;
    *counter += 1;
    Variable::new_unchecked(format!("__infer_{kind}_{n}"))
}

fn var_term(v: &Variable) -> TermPattern {
    TermPattern::Variable(v.clone())
}

fn named(iri: &str) -> NamedNodePattern {
    NamedNodePattern::NamedNode(NamedNode::new_unchecked(iri))
}

fn link(iri: &str) -> Ppe {
    Ppe::NamedNode(NamedNode::new_unchecked(iri))
}

fn one_or_more(iri: &str) -> Ppe {
    Ppe::OneOrMore(Box::new(link(iri)))
}

fn zero_or_more(iri: &str) -> Ppe {
    Ppe::ZeroOrMore(Box::new(link(iri)))
}

/// `rdfs:subPropertyOf*/<declaration>/rdfs:subClassOf*` — the predicate
/// hierarchy, the declaration, and the class hierarchy as one path, so a bound
/// class is walked backward from and neither closure enumerates the graph.
fn declaration_path(declaration: &str) -> Ppe {
    Ppe::Sequence(
        Box::new(Ppe::Sequence(
            Box::new(Ppe::ZeroOrMore(Box::new(link(RDFS_SUBPROPERTY_OF)))),
            Box::new(link(declaration)),
        )),
        Box::new(Ppe::ZeroOrMore(Box::new(link(RDFS_SUBCLASS_OF)))),
    )
}

fn is_type_predicate(p: &NamedNodePattern) -> bool {
    matches!(p, NamedNodePattern::NamedNode(nn) if nn.as_str() == RDF_TYPE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spargebra::SparqlParser;

    const TYPE_PATTERN: &str = "SELECT ?s WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Person> }";

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

    /// The parsed BGP of a single-pattern query.
    fn bgp_of(query_str: &str) -> Vec<TriplePattern> {
        match SparqlParser::new().parse_query(query_str).unwrap() {
            spargebra::Query::Select { pattern, .. } => match pattern {
                GP::Project { inner, .. } => match *inner {
                    GP::Bgp { patterns } => patterns,
                    other => panic!("expected a Bgp, got {other:?}"),
                },
                other => panic!("expected a Project, got {other:?}"),
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn subclasof_rule_expands_type_pattern() {
        let rewritten = rewrite_query(TYPE_PATTERN);
        assert!(
            rewritten.contains("UNION"),
            "expected UNION in rewritten query, got: {rewritten}"
        );
        assert!(
            rewritten.contains("http://www.w3.org/2000/01/rdf-schema#subClassOf"),
            "expected subClassOf reference, got: {rewritten}"
        );
    }

    #[test]
    fn subproperty_rewrites_non_type_pattern() {
        let rewritten = rewrite_query("SELECT ?s WHERE { ?s <http://example.org/knows> ?o }");
        assert!(
            rewritten.contains("subPropertyOf"),
            "expected subPropertyOf rewrite, got: {rewritten}"
        );
    }

    #[test]
    fn domain_rule_expands() {
        let rewritten = rewrite_query(TYPE_PATTERN);
        assert!(
            rewritten.contains("http://www.w3.org/2000/01/rdf-schema#domain"),
            "expected domain reference, got: {rewritten}"
        );
    }

    #[test]
    fn range_rule_expands() {
        let rewritten = rewrite_query(TYPE_PATTERN);
        assert!(
            rewritten.contains("http://www.w3.org/2000/01/rdf-schema#range"),
            "expected range reference, got: {rewritten}"
        );
    }

    #[test]
    fn preserves_non_type_patterns() {
        let rewritten = rewrite_query(
            "SELECT ?s ?name WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://example.org/Person> . ?s <http://example.org/name> ?name }",
        );
        assert!(
            rewritten.contains("http://example.org/name"),
            "expected original pattern to survive, got: {rewritten}"
        );
    }

    #[test]
    fn rewritten_bgp_is_deduplicated() {
        let rewritten = rewrite_query(TYPE_PATTERN);
        assert!(
            rewritten.contains("DISTINCT"),
            "expected the rewritten BGP to be deduplicated, got: {rewritten}"
        );
    }

    /// Four rules fire on an `rdf:type` pattern, one on anything else.
    #[test]
    fn branch_count_per_pattern() {
        assert_eq!(inference_branches(&bgp_of(TYPE_PATTERN), &mut 0).len(), 4);
        let plain = bgp_of("SELECT ?s WHERE { ?s <http://example.org/knows> ?o }");
        assert_eq!(inference_branches(&plain, &mut 0).len(), 1);
    }

    /// Every branch joins its schema path first, so an empty lookup can
    /// short-circuit before the data patterns are read.
    #[test]
    fn every_branch_leads_with_its_path() {
        for branch in inference_branches(&bgp_of(TYPE_PATTERN), &mut 0) {
            match branch.into_pattern() {
                GP::Join { left, .. } => assert!(matches!(*left, GP::Path { .. })),
                other => panic!("expected a Join, got {other:?}"),
            }
        }
    }

    /// The transitive rules are closures, not single hops.
    #[test]
    fn transitive_rules_are_closures() {
        let branches = inference_branches(&bgp_of(TYPE_PATTERN), &mut 0);
        assert!(matches!(branches[0].path.1, Ppe::OneOrMore(_)));
        for branch in &branches[1..3] {
            let Ppe::Sequence(head, tail) = &branch.path.1 else {
                panic!("expected a sequence path")
            };
            assert!(matches!(**tail, Ppe::ZeroOrMore(_)));
            let Ppe::Sequence(chain, _) = &**head else {
                panic!("expected a nested sequence")
            };
            assert!(matches!(**chain, Ppe::ZeroOrMore(_)));
        }
        // Sub-property-of-`rdf:type`: predicate closure, then the class
        // closure as a second path (zero hops allowed — the declared type
        // itself is entailed).
        let sub_property_type = &branches[3];
        assert!(matches!(sub_property_type.path.1, Ppe::OneOrMore(_)));
        let (_, second, _) = sub_property_type.second.as_ref().expect("second path");
        assert!(matches!(second, Ppe::ZeroOrMore(_)));
    }

    /// A path led by plain predicates is expanded into triple patterns, so the
    /// `rdf:type` step inside it gets the type branches.
    #[test]
    fn type_step_inside_a_path_is_rewritten() {
        let rewritten = rewrite_query(
            "SELECT ?s WHERE { ?s <http://www.w3.org/1999/02/22-rdf-syntax-ns#type>/<http://www.w3.org/2000/01/rdf-schema#subClassOf>* <http://example.org/Person> }",
        );
        assert!(
            rewritten.contains("subClassOf"),
            "expected the tail path to survive, got: {rewritten}"
        );
        assert!(
            rewritten.contains("UNION"),
            "expected the expanded rdf:type step to carry branches, got: {rewritten}"
        );
    }

    /// A path that does not lead with a plain predicate is left alone.
    #[test]
    fn operator_led_paths_are_kept() {
        let rewritten = rewrite_query(
            "SELECT ?s WHERE { ?s (<http://example.org/a>|<http://example.org/b>) <http://example.org/c> }",
        );
        assert!(
            !rewritten.contains("UNION"),
            "expected no rewrite for an alternative path, got: {rewritten}"
        );
    }
}
