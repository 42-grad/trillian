use rustc_hash::FxHashMap;

use super::planner::{ExecutionPlan, GraphPattern, PatternTerm, TriplePattern};
use super::query::{QueryResult, Term, TripleStore, Var};

/// Eine partielle oder vollständige Variablenbindung.
/// `values[i]` gehört zur Variable mit Index `i` im `var_map`.
#[derive(Debug, Clone)]
pub struct Binding {
    values: Vec<Option<u32>>,
}

impl Binding {
    pub fn new(n_vars: usize) -> Self {
        Self {
            values: vec![None; n_vars],
        }
    }

    #[inline]
    pub fn get(&self, idx: usize) -> Option<u32> {
        self.values[idx]
    }

    #[inline]
    pub fn set(&mut self, idx: usize, val: u32) {
        self.values[idx] = Some(val);
    }

    #[inline]
    pub fn is_bound(&self, idx: usize) -> bool {
        self.values[idx].is_some()
    }

    /// Liefert alle Werte als flachen Vec – nützlich für finale Ergebnisse.
    pub fn into_row(self) -> Vec<u32> {
        self.values.into_iter().map(|v| v.unwrap()).collect()
    }
}

// ---------------------------------------------------------------------------
// Klassischer planbasierter Executor (für nicht-binäre Muster als Fallback)
// ---------------------------------------------------------------------------

/// Führt einen `ExecutionPlan` für ein `GraphPattern` aus.
pub fn execute_plan(
    store: &TripleStore,
    pattern: &GraphPattern,
    plan: &ExecutionPlan,
) -> Vec<Vec<u32>> {
    if plan.steps.is_empty() {
        return Vec::new();
    }

    let mut var_map: FxHashMap<String, usize> = FxHashMap::default();
    for pat in &pattern.patterns {
        collect_vars(&pat.subject, &mut var_map);
        collect_vars(&pat.predicate, &mut var_map);
        collect_vars(&pat.object, &mut var_map);
    }
    let n_vars = var_map.len();

    let mut results: Vec<Binding> = Vec::new();

    for (step_idx, step) in plan.steps.iter().enumerate() {
        let triple_pattern = &pattern.patterns[step.pattern_index];

        if step_idx == 0 {
            results = execute_pattern(store, triple_pattern, None, &var_map, n_vars);
        } else {
            let mut new_results = Vec::new();
            for binding in &results {
                let extensions = execute_pattern(
                    store,
                    triple_pattern,
                    Some(binding.clone()),
                    &var_map,
                    n_vars,
                );
                new_results.extend(extensions);
            }
            results = new_results;
        }
    }

    results.into_iter().map(|b| b.into_row()).collect()
}

fn collect_vars(term: &PatternTerm, var_map: &mut FxHashMap<String, usize>) {
    if let PatternTerm::Variable(name) = term {
        if !var_map.contains_key(name) {
            let id = var_map.len();
            var_map.insert(name.clone(), id);
        }
    }
}

fn execute_pattern(
    store: &TripleStore,
    pattern: &TriplePattern,
    prior: Option<Binding>,
    var_map: &FxHashMap<String, usize>,
    n_vars: usize,
) -> Vec<Binding> {
    let eff_s = effective_term(&pattern.subject, prior.as_ref(), var_map);
    let eff_p = effective_term(&pattern.predicate, prior.as_ref(), var_map);
    let eff_o = effective_term(&pattern.object, prior.as_ref(), var_map);

    let mut out = Vec::new();

    match store.query(eff_s, eff_p, eff_o) {
        QueryResult::Exact(true) => {
            out.push(prior.unwrap_or_else(|| Binding::new(n_vars)));
        }
        QueryResult::Exact(false) | QueryResult::Empty => {}

        QueryResult::Single(var, values) => {
            let var_name = pattern_var_name(pattern, var);
            let var_idx = var_map[var_name];
            for &val in values {
                let mut binding = prior.clone().unwrap_or_else(|| Binding::new(n_vars));
                binding.set(var_idx, val);
                out.push(binding);
            }
        }

        QueryResult::Double(var1, var2, pairs) => {
            let name1 = pattern_var_name(pattern, var1);
            let name2 = pattern_var_name(pattern, var2);
            let idx1 = var_map[name1];
            let idx2 = var_map[name2];
            for (val1, val2) in pairs {
                let mut binding = prior.clone().unwrap_or_else(|| Binding::new(n_vars));
                binding.set(idx1, val1);
                binding.set(idx2, val2);
                out.push(binding);
            }
        }

        QueryResult::All(triples) => {
            let s_idx = pattern.subject.as_variable().and_then(|n| var_map.get(n)).copied();
            let p_idx = pattern
                .predicate
                .as_variable()
                .and_then(|n| var_map.get(n))
                .copied();
            let o_idx = pattern.object.as_variable().and_then(|n| var_map.get(n)).copied();

            for (s, p, o) in triples {
                let mut binding = Binding::new(n_vars);
                if let Some(idx) = s_idx {
                    binding.set(idx, s);
                }
                if let Some(idx) = p_idx {
                    binding.set(idx, p);
                }
                if let Some(idx) = o_idx {
                    binding.set(idx, o);
                }
                out.push(binding);
            }
        }
    }

    out
}

fn effective_term(
    term: &PatternTerm,
    prior: Option<&Binding>,
    var_map: &FxHashMap<String, usize>,
) -> Term {
    match term {
        PatternTerm::Bound(id) => Term::Bound(*id),
        PatternTerm::Variable(name) => {
            if let Some(binding) = prior {
                let idx = var_map[name];
                if let Some(val) = binding.get(idx) {
                    Term::Bound(val)
                } else {
                    Term::Wildcard
                }
            } else {
                Term::Wildcard
            }
        }
    }
}

fn pattern_var_name(pattern: &TriplePattern, var: Var) -> &String {
    let term = match var {
        Var::S => &pattern.subject,
        Var::P => &pattern.predicate,
        Var::O => &pattern.object,
    };
    term.as_variable().expect("result variable expected")
}

impl PatternTerm {
    fn as_variable(&self) -> Option<&String> {
        match self {
            PatternTerm::Variable(name) => Some(name),
            PatternTerm::Bound(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// WCOJ / Leapfrog Triejoin Executor
// ---------------------------------------------------------------------------

/// Führt ein Graph-Pattern mit Worst-Case-Optimal Join aus.
///
/// Aktuell optimiert für binäre Muster `(?X, P, ?Y)` mit gebundenem
/// Prädikat P (das Dreiecks-Szenario). Andere Muster werden transparent
/// an den klassischen planbasierten Executor delegiert.
pub fn execute_wcoj(store: &TripleStore, pattern: &GraphPattern) -> Vec<Vec<u32>> {
    if !is_wcoj_applicable(pattern, store) {
        let plan = pattern.optimize(&store.stats);
        return execute_plan(store, pattern, &plan);
    }

    let var_order = determine_variable_order(pattern);
    let var_map: FxHashMap<String, usize> = var_order
        .iter()
        .enumerate()
        .map(|(i, v)| (v.clone(), i))
        .collect();

    let mut results: Vec<Vec<u32>> = Vec::new();
    let mut binding = Binding::new(var_order.len());

    wcoj_recurse(
        store,
        pattern,
        &var_order,
        &var_map,
        0,
        &mut binding,
        &mut results,
        true, // erste Ebene parallel
    );

    results
}

fn is_wcoj_applicable(pattern: &GraphPattern, store: &TripleStore) -> bool {
    pattern.patterns.iter().all(|pat| {
        // Genau ein gebundenes Prädikat und zwei Variablen an Subjekt/Objekt
        let pred_bound = matches!(pat.predicate, PatternTerm::Bound(pid) if store.has_predicate(pid));
        let two_vars = [pat.subject.is_variable(), pat.object.is_variable()]
            .iter()
            .filter(|&&x| x)
            .count()
            == 2;
        pred_bound && two_vars
    })
}

fn determine_variable_order(pattern: &GraphPattern) -> Vec<String> {
    // Heuristik: Variablen in ihrer ersten Erscheinungsreihenfolge.
    // Für symmetrische Muster (Dreieck) ist die Reihenfolge nicht kritisch.
    let mut seen = FxHashMap::default();
    let mut order = Vec::new();
    for pat in &pattern.patterns {
        for name in pat.variables() {
            if !seen.contains_key(name) {
                seen.insert(name.clone(), ());
                order.push(name.clone());
            }
        }
    }
    order
}

fn wcoj_recurse(
    store: &TripleStore,
    pattern: &GraphPattern,
    var_order: &[String],
    var_map: &FxHashMap<String, usize>,
    depth: usize,
    binding: &mut Binding,
    results: &mut Vec<Vec<u32>>,
    parallel: bool,
) {
    if depth == var_order.len() {
        results.push(binding.clone().into_row());
        return;
    }

    let var_name = &var_order[depth];
    let candidates = leapfrog_candidates(store, pattern, var_name, binding, var_map);

    if candidates.is_empty() {
        return;
    }

    if parallel && depth == 0 {
        use rayon::prelude::*;

        let local_results: Vec<Vec<Vec<u32>>> = candidates
            .par_iter()
            .map(|&val| {
                let mut local_binding = Binding::new(var_order.len());
                local_binding.set(var_map[var_name], val);
                let mut local_results = Vec::new();
                wcoj_recurse(
                    store,
                    pattern,
                    var_order,
                    var_map,
                    depth + 1,
                    &mut local_binding,
                    &mut local_results,
                    false,
                );
                local_results
            })
            .collect();

        for mut r in local_results {
            results.append(&mut r);
        }
    } else {
        for &val in &candidates {
            binding.set(var_map[var_name], val);
            wcoj_recurse(
                store,
                pattern,
                var_order,
                var_map,
                depth + 1,
                binding,
                results,
                false,
            );
        }
    }
}

/// Schnittmenge der Kandidaten für `var_name` über alle betroffenen Muster.
fn leapfrog_candidates(
    store: &TripleStore,
    pattern: &GraphPattern,
    var_name: &str,
    binding: &Binding,
    var_map: &FxHashMap<String, usize>,
) -> Vec<u32> {
    let mut slices: Vec<&[u32]> = Vec::new();

    for pat in &pattern.patterns {
        if let Some(slice) = pattern_slice_for_var(store, pat, var_name, binding, var_map) {
            slices.push(slice);
        }
    }

    leapfrog_intersect(&slices)
}

/// Liefert den sortierten Kandidaten-Slice für eine Variable in einem Muster,
/// unter Berücksichtigung bereits gebundener Variablen.
fn pattern_slice_for_var<'a>(
    store: &'a TripleStore,
    pat: &'a TriplePattern,
    var_name: &str,
    binding: &Binding,
    var_map: &FxHashMap<String, usize>,
) -> Option<&'a [u32]> {
    // Nur binäre Muster mit gebundenem Prädikat
    let pid = match pat.predicate {
        PatternTerm::Bound(pid) => pid,
        _ => return None,
    };
    if !store.has_predicate(pid) {
        return None;
    }

    let var_at_subject = pat.subject.variable_name() == Some(var_name);
    let var_at_object = pat.object.variable_name() == Some(var_name);

    if var_at_subject {
        // Subjekt-Kandidaten; falls Objekt gebunden, über POS einschränken,
        // sonst alle distinkten Subjekte des Prädikats.
        if let Some(bound_obj) = pat.object.bound_or_resolved(binding, var_map) {
            Some(store.subjects_of(pid, bound_obj))
        } else {
            Some(store.subjects_with_predicate(pid))
        }
    } else if var_at_object {
        // Objekt-Kandidaten; falls Subjekt gebunden, über SPO einschränken,
        // sonst alle distinkten Objekte des Prädikats.
        if let Some(bound_sub) = pat.subject.bound_or_resolved(binding, var_map) {
            Some(store.objects_of(bound_sub, pid))
        } else {
            Some(store.objects_with_predicate(pid))
        }
    } else {
        None
    }
}

/// Leapfrog-Intersektion mehrerer sortierter Slices.
fn leapfrog_intersect(slices: &[&[u32]]) -> Vec<u32> {
    if slices.is_empty() {
        return Vec::new();
    }
    if slices.len() == 1 {
        return slices[0].to_vec();
    }

    // Starte mit dem kleinsten Slice
    let mut idx = 0usize;
    let mut min_len = usize::MAX;
    for (i, s) in slices.iter().enumerate() {
        if s.len() < min_len {
            min_len = s.len();
            idx = i;
        }
    }
    let (first, rest_with_idx) = {
        let mut rest = Vec::new();
        for (i, s) in slices.iter().enumerate() {
            if i == idx {
                continue;
            }
            rest.push((i, *s));
        }
        (slices[idx], rest)
    };

    let mut result = Vec::new();
    'outer: for &candidate in first {
        for (_, s) in &rest_with_idx {
            if s.binary_search(&candidate).is_err() {
                continue 'outer;
            }
        }
        result.push(candidate);
    }

    result
}

// Hilfs-Traits für PatternTerm
impl PatternTerm {
    pub fn is_variable(&self) -> bool {
        matches!(self, PatternTerm::Variable(_))
    }

    pub fn variable_name(&self) -> Option<&str> {
        match self {
            PatternTerm::Variable(name) => Some(name.as_str()),
            PatternTerm::Bound(_) => None,
        }
    }

    /// Falls der Term eine Konstante ist oder eine Variable, die bereits
    /// in `binding` gebunden ist, wird der konkrete u32-Wert zurückgegeben.
    pub fn bound_or_resolved(
        &self,
        binding: &Binding,
        var_map: &FxHashMap<String, usize>,
    ) -> Option<u32> {
        match self {
            PatternTerm::Bound(id) => Some(*id),
            PatternTerm::Variable(name) => {
                let idx = var_map.get(name)?;
                binding.get(*idx)
            }
        }
    }
}

#[cfg(test)]
mod wcoj_tests {
    use super::*;

    #[test]
    fn wcoj_triangle() {
        // Dreieck: alice-bob-charlie-alice
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            ("alice", "knows", "bob"),
            ("bob", "knows", "charlie"),
            ("charlie", "knows", "alice"),
            ("alice", "knows", "dave"), // kein Dreieck
        ]);

        let knows = store.dict.lookup("knows").unwrap();
        let pattern = GraphPattern {
            patterns: vec![
                TriplePattern {
                    subject: PatternTerm::Variable("a".to_string()),
                    predicate: PatternTerm::Bound(knows),
                    object: PatternTerm::Variable("b".to_string()),
                },
                TriplePattern {
                    subject: PatternTerm::Variable("b".to_string()),
                    predicate: PatternTerm::Bound(knows),
                    object: PatternTerm::Variable("c".to_string()),
                },
                TriplePattern {
                    subject: PatternTerm::Variable("c".to_string()),
                    predicate: PatternTerm::Bound(knows),
                    object: PatternTerm::Variable("a".to_string()),
                },
            ],
        };

        let results = execute_wcoj(&store, &pattern);
        // Jede Rotation des Dreiecks ist ein Ergebnis: (a,b,c), (b,c,a), (c,a,b)
        assert_eq!(results.len(), 3);
    }
}
