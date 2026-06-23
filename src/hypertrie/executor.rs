use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustc_hash::FxHashMap;

use super::planner::{ExecutionPlan, GraphPattern, PatternTerm, TriplePattern};
use super::query::{QueryResult, Term, TripleStore, Var};

/// Sentinel für eine noch ungebundene Variable in einer (partiellen) Zeile.
/// Alias auf die zentrale [`super::NULL_ID`]-Konstante.
pub const UNBOUND: u32 = super::NULL_ID;

/// Obergrenze für materialisierte Ergebniszeilen. Schützt den Server davor, bei
/// einer entarteten Query (Kreuzprodukt disjunkter Muster oder unbeschränkt
/// großer Zwischen-Join) den gesamten RAM zu allozieren und vom OOM-Killer
/// beendet zu werden. Per `TRILLIAN_MAX_ROWS` überschreibbar.
pub fn max_result_rows() -> usize {
    use std::sync::OnceLock;
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("TRILLIAN_MAX_ROWS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20_000_000)
    })
}

/// Flache, zeilen-orientierte Ergebnis-Matrix.
///
/// Statt `Vec<Vec<u32>>` (eine Heap-Allokation **pro Zeile**) liegen alle
/// Zeilen row-major in **einem** `Vec<u32>`. Das eliminiert die Millionen
/// kleiner Allokationen, die zuvor die Query-Latenz und den Query-Peak-Speicher
/// dominierten.
#[derive(Debug, Clone, Default)]
pub struct RowBlock {
    n_vars: usize,
    n_rows: usize,
    data: Vec<u32>,
}

impl RowBlock {
    pub fn new(n_vars: usize) -> Self {
        Self {
            n_vars,
            n_rows: 0,
            data: Vec::new(),
        }
    }

    #[inline]
    pub fn n_vars(&self) -> usize {
        self.n_vars
    }

    #[inline]
    pub fn n_rows(&self) -> usize {
        self.n_rows
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.n_rows == 0
    }

    #[inline]
    pub fn row(&self, i: usize) -> &[u32] {
        let s = i * self.n_vars;
        &self.data[s..s + self.n_vars]
    }

    pub fn rows(&self) -> RowIter<'_> {
        RowIter { block: self, i: 0 }
    }

    /// Hängt eine Zeile an (Breite muss `n_vars` sein).
    #[inline]
    pub fn push_row(&mut self, row: &[u32]) {
        debug_assert_eq!(row.len(), self.n_vars);
        self.data.extend_from_slice(row);
        self.n_rows += 1;
    }

    /// Beginnt eine neue Zeile als Kopie von `prior` (oder ganz `UNBOUND`) und
    /// liefert deren Start-Offset, damit der Aufrufer einzelne Spalten setzt –
    /// ohne temporäre Zeilen-`Vec`.
    #[inline]
    fn push_from_prior(&mut self, prior: Option<&[u32]>) -> usize {
        let start = self.data.len();
        match prior {
            Some(p) => self.data.extend_from_slice(p),
            None => self.data.resize(start + self.n_vars, UNBOUND),
        }
        self.n_rows += 1;
        start
    }

    /// Hängt eine Zeile aus `prefix` an, mit `fill` auf `n_vars` aufgefüllt.
    pub fn push_row_padded(&mut self, prefix: &[u32], fill: u32) {
        debug_assert!(prefix.len() <= self.n_vars);
        self.data.extend_from_slice(prefix);
        let pad = self.n_vars - prefix.len();
        self.data.resize(self.data.len() + pad, fill);
        self.n_rows += 1;
    }

    /// Hängt eine Zeile als Verkettung `prefix ++ suffix` an.
    pub fn push_row_concat(&mut self, prefix: &[u32], suffix: &[u32]) {
        debug_assert_eq!(prefix.len() + suffix.len(), self.n_vars);
        self.data.extend_from_slice(prefix);
        self.data.extend_from_slice(suffix);
        self.n_rows += 1;
    }

    /// Hängt alle Zeilen eines anderen Blocks an (gleiche Breite).
    pub fn append(&mut self, other: &RowBlock) {
        debug_assert_eq!(self.n_vars, other.n_vars);
        self.data.extend_from_slice(&other.data);
        self.n_rows += other.n_rows;
    }

    /// Neuer Block mit nur den ausgewählten Spalten (in der gegebenen Reihenfolge).
    pub fn project(&self, indices: &[usize]) -> RowBlock {
        let mut out = RowBlock::new(indices.len());
        out.data.reserve(self.n_rows * indices.len());
        for r in self.rows() {
            for &i in indices {
                out.data.push(r[i]);
            }
        }
        out.n_rows = self.n_rows;
        out
    }

    /// Sortiert die Zeilen und entfernt Duplikate (SPARQL `DISTINCT`).
    pub fn sort_distinct(&mut self) {
        if self.n_vars == 0 {
            self.n_rows = self.n_rows.min(1);
            return;
        }
        let mut idx: Vec<usize> = (0..self.n_rows).collect();
        idx.sort_unstable_by(|&a, &b| self.row(a).cmp(self.row(b)));
        let mut new_data: Vec<u32> = Vec::with_capacity(self.data.len());
        let mut new_rows = 0usize;
        let mut prev: Option<usize> = None;
        for &i in &idx {
            if let Some(p) = prev
                && self.row(p) == self.row(i)
            {
                continue;
            }
            new_data.extend_from_slice(self.row(i));
            new_rows += 1;
            prev = Some(i);
        }
        self.data = new_data;
        self.n_rows = new_rows;
    }

    /// Entfernt Duplikate **ohne** Umsortierung (für DISTINCT nach ORDER BY).
    pub fn dedup_preserving_order(&mut self) {
        if self.n_vars == 0 {
            self.n_rows = self.n_rows.min(1);
            return;
        }
        let mut seen: std::collections::HashSet<Vec<u32>> = std::collections::HashSet::new();
        let mut new_data: Vec<u32> = Vec::with_capacity(self.data.len());
        let mut new_rows = 0usize;
        for i in 0..self.n_rows {
            let row = self.row(i);
            if seen.insert(row.to_vec()) {
                new_data.extend_from_slice(row);
                new_rows += 1;
            }
        }
        self.data = new_data;
        self.n_rows = new_rows;
    }

    /// Wendet OFFSET/LIMIT an (in Zeilen).
    pub fn apply_offset_limit(&mut self, offset: usize, limit: Option<usize>) {
        let start = offset.min(self.n_rows);
        let end = match limit {
            Some(l) => (start + l).min(self.n_rows),
            None => self.n_rows,
        };
        if self.n_vars > 0 {
            self.data = self.data[start * self.n_vars..end * self.n_vars].to_vec();
        }
        self.n_rows = end - start;
    }
}

pub struct RowIter<'a> {
    block: &'a RowBlock,
    i: usize,
}

impl<'a> Iterator for RowIter<'a> {
    type Item = &'a [u32];
    fn next(&mut self) -> Option<&'a [u32]> {
        if self.i >= self.block.n_rows {
            return None;
        }
        let r = self.block.row(self.i);
        self.i += 1;
        Some(r)
    }
}

// ---------------------------------------------------------------------------
// Klassischer planbasierter Executor (für nicht-binäre Muster als Fallback)
// ---------------------------------------------------------------------------

/// Führt einen `ExecutionPlan` für ein `GraphPattern` aus.
///
/// **Pipelined (DFS):** statt jede Join-Ebene vollständig zu materialisieren,
/// wird je Teil-Zeile tiefenrekursiv bis zur fertigen Zeile durchgereicht. Damit
/// bleibt der Speicher auf ~Endzeilen + Rekursionstiefe beschränkt (kein
/// aufgeblähter Zwischen-Join), und ein `limit` terminiert früh — exakt das, was
/// WDBench (Output-Cap 100k) misst. `limit=None` produziert alles bis zum Cap.
pub fn execute_plan(
    store: &TripleStore,
    pattern: &GraphPattern,
    plan: &ExecutionPlan,
) -> Result<RowBlock, String> {
    execute_plan_limited(store, pattern, plan, None)
}

pub fn execute_plan_limited(
    store: &TripleStore,
    pattern: &GraphPattern,
    plan: &ExecutionPlan,
    limit: Option<usize>,
) -> Result<RowBlock, String> {
    let mut var_map: FxHashMap<String, usize> = FxHashMap::default();
    for pat in &pattern.patterns {
        collect_vars(&pat.subject, &mut var_map);
        collect_vars(&pat.predicate, &mut var_map);
        collect_vars(&pat.object, &mut var_map);
    }
    let n_vars = var_map.len();
    let cap = max_result_rows();
    // Sobald so viele Zeilen da sind, wird gestoppt: das LIMIT (sauberer Stopp)
    // oder cap+1 (dann meldet der Aufrufer "too large"). saturating gegen MAX.
    let stop = limit
        .map(|l| l.min(cap.saturating_add(1)))
        .unwrap_or(cap.saturating_add(1));

    let mut out = RowBlock::new(n_vars);
    if plan.steps.is_empty() {
        return Ok(out);
    }

    // Seed: selektivstes (erstes) Muster materialisieren. Der Planner wählt es
    // als das selektivste -> klein; ein entarteter Voll-Scan wird per Cap erfasst.
    let mut seed = RowBlock::new(n_vars);
    extend_pattern(
        store,
        &pattern.patterns[plan.steps[0].pattern_index],
        None,
        &var_map,
        n_vars,
        &mut seed,
    );
    if seed.n_rows() > cap {
        return Err(result_too_large(cap));
    }

    if plan.steps.len() == 1 {
        // Einzelmuster: direkt bis `stop` übernehmen.
        for i in 0..seed.n_rows().min(stop) {
            out.push_row(seed.row(i));
        }
    } else {
        for i in 0..seed.n_rows() {
            if out.n_rows() >= stop {
                break;
            }
            plan_dfs(
                store,
                pattern,
                plan,
                &var_map,
                n_vars,
                seed.row(i),
                1,
                &mut out,
                stop,
            );
        }
    }

    if out.n_rows() > cap {
        return Err(result_too_large(cap));
    }
    Ok(out)
}

/// Reicht eine Teil-Zeile durch die restlichen Plan-Schritte (Tiefensuche) und
/// hängt fertige Zeilen an `out` an, bis `stop` erreicht ist.
#[allow(clippy::too_many_arguments)]
fn plan_dfs(
    store: &TripleStore,
    pattern: &GraphPattern,
    plan: &ExecutionPlan,
    var_map: &FxHashMap<String, usize>,
    n_vars: usize,
    partial: &[u32],
    step: usize,
    out: &mut RowBlock,
    stop: usize,
) {
    if out.n_rows() >= stop {
        return;
    }
    if step == plan.steps.len() {
        out.push_row(partial);
        return;
    }
    // Erweiterungen der Teil-Zeile um EIN Muster (beschränkt durch den Fan-out
    // dieses Knotens, nicht durch die globale Zwischenmenge).
    let mut tmp = RowBlock::new(n_vars);
    extend_pattern(
        store,
        &pattern.patterns[plan.steps[step].pattern_index],
        Some(partial),
        var_map,
        n_vars,
        &mut tmp,
    );
    for j in 0..tmp.n_rows() {
        if out.n_rows() >= stop {
            break;
        }
        // `tmp` wird in der Rekursion nicht mutiert -> Slice direkt durchreichen.
        plan_dfs(
            store,
            pattern,
            plan,
            var_map,
            n_vars,
            tmp.row(j),
            step + 1,
            out,
            stop,
        );
    }
}

fn result_too_large(cap: usize) -> String {
    format!(
        "result exceeds {} rows (likely an unbounded/cross-product query); \
         raise TRILLIAN_MAX_ROWS to allow",
        cap
    )
}

fn collect_vars(term: &PatternTerm, var_map: &mut FxHashMap<String, usize>) {
    if let PatternTerm::Variable(name) = term
        && !var_map.contains_key(name)
    {
        let id = var_map.len();
        var_map.insert(name.clone(), id);
    }
}

/// Wertet ein Muster gegen den Store aus und schreibt die (erweiterten) Zeilen
/// direkt in `out` – ohne temporäre Zeilen-`Vec` pro Ergebniszeile.
fn extend_pattern(
    store: &TripleStore,
    pattern: &TriplePattern,
    prior: Option<&[u32]>,
    var_map: &FxHashMap<String, usize>,
    n_vars: usize,
    out: &mut RowBlock,
) {
    let _ = n_vars;
    let eff_s = effective_term(&pattern.subject, prior, var_map);
    let eff_p = effective_term(&pattern.predicate, prior, var_map);
    let eff_o = effective_term(&pattern.object, prior, var_map);

    match store.query(eff_s, eff_p, eff_o) {
        QueryResult::Exact(true) => {
            out.push_from_prior(prior);
        }
        QueryResult::Exact(false) | QueryResult::Empty => {}

        QueryResult::Single(var, values) => {
            let var_idx = var_map[pattern_var_name(pattern, var)];
            for &val in values.iter() {
                let start = out.push_from_prior(prior);
                out.data[start + var_idx] = val;
            }
        }

        QueryResult::Double(var1, var2, pairs) => {
            let idx1 = var_map[pattern_var_name(pattern, var1)];
            let idx2 = var_map[pattern_var_name(pattern, var2)];
            for (val1, val2) in pairs {
                let start = out.push_from_prior(prior);
                out.data[start + idx1] = val1;
                out.data[start + idx2] = val2;
            }
        }

        QueryResult::All(triples) => {
            let s_idx = pattern
                .subject
                .as_variable()
                .and_then(|n| var_map.get(n))
                .copied();
            let p_idx = pattern
                .predicate
                .as_variable()
                .and_then(|n| var_map.get(n))
                .copied();
            let o_idx = pattern
                .object
                .as_variable()
                .and_then(|n| var_map.get(n))
                .copied();

            for (s, p, o) in triples {
                let start = out.push_from_prior(prior);
                if let Some(idx) = s_idx {
                    out.data[start + idx] = s;
                }
                if let Some(idx) = p_idx {
                    out.data[start + idx] = p;
                }
                if let Some(idx) = o_idx {
                    out.data[start + idx] = o;
                }
            }
        }
    }
}

fn effective_term(
    term: &PatternTerm,
    prior: Option<&[u32]>,
    var_map: &FxHashMap<String, usize>,
) -> Term {
    match term {
        PatternTerm::Bound(id) => Term::Bound(*id),
        PatternTerm::Variable(name) => {
            if let Some(row) = prior {
                let v = row[var_map[name]];
                if v != UNBOUND {
                    Term::Bound(v)
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
pub fn execute_wcoj(store: &TripleStore, pattern: &GraphPattern) -> Result<RowBlock, String> {
    execute_wcoj_limited(store, pattern, None)
}

pub fn execute_wcoj_limited(
    store: &TripleStore,
    pattern: &GraphPattern,
    limit: Option<usize>,
) -> Result<RowBlock, String> {
    if !is_wcoj_applicable(pattern, store) {
        let plan = pattern.optimize(store);
        return execute_plan_limited(store, pattern, &plan, limit);
    }

    let var_order = determine_variable_order(pattern);
    let var_map: FxHashMap<String, usize> = var_order
        .iter()
        .enumerate()
        .map(|(i, v)| (v.clone(), i))
        .collect();

    let n = var_order.len();
    let mut results = RowBlock::new(n);
    let mut binding = vec![UNBOUND; n];
    let cap = max_result_rows();
    // Stopp-Schwelle: LIMIT (sauberer Früh-Stopp) oder cap+1 (dann meldet der
    // Aufrufer "too large"). Bremst die (parallele) Rekursion SOFORT statt
    // nachträglich -> kein OOM.
    let stop = limit
        .map(|l| l.min(cap.saturating_add(1)))
        .unwrap_or(cap.saturating_add(1));
    let produced = AtomicUsize::new(0);

    wcoj_recurse(
        store,
        pattern,
        &var_order,
        &var_map,
        0,
        &mut binding,
        &mut results,
        true, // erste Ebene parallel
        stop,
        &produced,
    );

    if produced.load(Ordering::Relaxed) > cap {
        return Err(result_too_large(cap));
    }
    Ok(results)
}

fn is_wcoj_applicable(pattern: &GraphPattern, store: &TripleStore) -> bool {
    pattern.patterns.iter().all(|pat| {
        // Genau ein gebundenes Prädikat und zwei Variablen an Subjekt/Objekt
        let pred_bound =
            matches!(pat.predicate, PatternTerm::Bound(pid) if store.has_predicate(pid));
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

#[allow(clippy::too_many_arguments)]
fn wcoj_recurse(
    store: &TripleStore,
    pattern: &GraphPattern,
    var_order: &[String],
    var_map: &FxHashMap<String, usize>,
    depth: usize,
    binding: &mut Vec<u32>,
    results: &mut RowBlock,
    parallel: bool,
    cap: usize,
    produced: &AtomicUsize,
) {
    // Globaler Abbruch, sobald der Cap erreicht ist (von irgendeinem Zweig).
    if produced.load(Ordering::Relaxed) > cap {
        return;
    }

    if depth == var_order.len() {
        results.push_row(binding);
        produced.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let var_name = &var_order[depth];
    let candidates = leapfrog_candidates(store, pattern, var_name, binding, var_map);

    if candidates.is_empty() {
        return;
    }

    if parallel && depth == 0 {
        use rayon::prelude::*;

        let blocks: Vec<RowBlock> = candidates
            .par_iter()
            .map(|&val| {
                let mut local_binding = vec![UNBOUND; var_order.len()];
                local_binding[var_map[var_name]] = val;
                let mut local_results = RowBlock::new(var_order.len());
                wcoj_recurse(
                    store,
                    pattern,
                    var_order,
                    var_map,
                    depth + 1,
                    &mut local_binding,
                    &mut local_results,
                    false,
                    cap,
                    produced,
                );
                local_results
            })
            .collect();

        for b in &blocks {
            results.append(b);
        }
    } else {
        for &val in &candidates {
            if produced.load(Ordering::Relaxed) > cap {
                break;
            }
            binding[var_map[var_name]] = val;
            wcoj_recurse(
                store,
                pattern,
                var_order,
                var_map,
                depth + 1,
                binding,
                results,
                false,
                cap,
                produced,
            );
        }
    }
}

/// Schnittmenge der Kandidaten für `var_name` über alle betroffenen Muster.
fn leapfrog_candidates(
    store: &TripleStore,
    pattern: &GraphPattern,
    var_name: &str,
    binding: &[u32],
    var_map: &FxHashMap<String, usize>,
) -> Vec<u32> {
    let mut slices: Vec<Cow<[u32]>> = Vec::new();

    for pat in &pattern.patterns {
        if let Some(slice) = pattern_slice_for_var(store, pat, var_name, binding, var_map) {
            slices.push(slice);
        }
    }

    let refs: Vec<&[u32]> = slices.iter().map(|c| c.as_ref()).collect();
    leapfrog_intersect(&refs)
}

/// Liefert den sortierten Kandidaten-Slice für eine Variable in einem Muster.
fn pattern_slice_for_var<'a>(
    store: &'a TripleStore,
    pat: &'a TriplePattern,
    var_name: &str,
    binding: &[u32],
    var_map: &FxHashMap<String, usize>,
) -> Option<Cow<'a, [u32]>> {
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
        if let Some(bound_obj) = pat.object.bound_or_resolved(binding, var_map) {
            Some(store.subjects_of(pid, bound_obj))
        } else {
            Some(Cow::Borrowed(store.subjects_with_predicate(pid)))
        }
    } else if var_at_object {
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

// Hilfs-Methoden für PatternTerm
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

    /// Konkreter u32-Wert, falls Konstante oder bereits gebundene Variable.
    pub fn bound_or_resolved(
        &self,
        binding: &[u32],
        var_map: &FxHashMap<String, usize>,
    ) -> Option<u32> {
        match self {
            PatternTerm::Bound(id) => Some(*id),
            PatternTerm::Variable(name) => {
                let idx = *var_map.get(name)?;
                let v = binding[idx];
                if v != UNBOUND { Some(v) } else { None }
            }
        }
    }
}

#[cfg(test)]
mod wcoj_tests {
    use super::*;

    #[test]
    fn wcoj_triangle() {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            ("alice", "knows", "bob"),
            ("bob", "knows", "charlie"),
            ("charlie", "knows", "alice"),
            ("alice", "knows", "dave"), // kein Dreieck
        ]);

        let knows = store.dict.lookup_iri("knows").unwrap();
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

        let results = execute_wcoj(&store, &pattern).unwrap();
        // Jede Rotation des Dreiecks ist ein Ergebnis: (a,b,c), (b,c,a), (c,a,b)
        assert_eq!(results.n_rows(), 3);
    }
}
