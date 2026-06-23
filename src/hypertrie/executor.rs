use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustc_hash::FxHashMap;

use super::planner::{ExecutionPlan, GraphPattern, PatternTerm, TriplePattern};
use super::query::{QueryResult, Term, TripleStore, Var};

/// Sentinel for a still-unbound variable in a (partial) row.
/// Alias for the central [`super::NULL_ID`] constant.
pub const UNBOUND: u32 = super::NULL_ID;

/// Upper bound on materialized result rows. Protects the server from allocating
/// all of RAM and being killed by the OOM killer on a degenerate query (cross
/// product of disjoint patterns or an unbounded intermediate join). Overridable
/// via `TRILLIAN_MAX_ROWS`.
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

/// Flat, row-oriented result matrix.
///
/// Instead of `Vec<Vec<u32>>` (one heap allocation **per row**), all rows live
/// row-major in **a single** `Vec<u32>`. This eliminates the millions of small
/// allocations that previously dominated query latency and query peak memory.
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

    /// Appends a row (width must be `n_vars`).
    #[inline]
    pub fn push_row(&mut self, row: &[u32]) {
        debug_assert_eq!(row.len(), self.n_vars);
        self.data.extend_from_slice(row);
        self.n_rows += 1;
    }

    /// Starts a new row as a copy of `prior` (or all `UNBOUND`) and returns its
    /// start offset, so the caller can set individual columns – without a
    /// temporary row `Vec`.
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

    /// Appends a row from `prefix`, padded to `n_vars` with `fill`.
    pub fn push_row_padded(&mut self, prefix: &[u32], fill: u32) {
        debug_assert!(prefix.len() <= self.n_vars);
        self.data.extend_from_slice(prefix);
        let pad = self.n_vars - prefix.len();
        self.data.resize(self.data.len() + pad, fill);
        self.n_rows += 1;
    }

    /// Appends a row as the concatenation `prefix ++ suffix`.
    pub fn push_row_concat(&mut self, prefix: &[u32], suffix: &[u32]) {
        debug_assert_eq!(prefix.len() + suffix.len(), self.n_vars);
        self.data.extend_from_slice(prefix);
        self.data.extend_from_slice(suffix);
        self.n_rows += 1;
    }

    /// Appends all rows of another block (same width).
    pub fn append(&mut self, other: &RowBlock) {
        debug_assert_eq!(self.n_vars, other.n_vars);
        self.data.extend_from_slice(&other.data);
        self.n_rows += other.n_rows;
    }

    /// New block with only the selected columns (in the given order).
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

    /// Sorts the rows and removes duplicates (SPARQL `DISTINCT`).
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

    /// Removes duplicates **without** reordering (for DISTINCT after ORDER BY).
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

    /// Applies OFFSET/LIMIT (in rows).
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
// Classic plan-based executor (fallback for non-binary patterns)
// ---------------------------------------------------------------------------

/// Executes an `ExecutionPlan` for a `GraphPattern`.
///
/// **Pipelined (DFS):** instead of fully materializing each join level, every
/// partial row is passed down depth-first to a complete row. This keeps memory
/// bounded to ~final rows + recursion depth (no bloated intermediate join), and
/// a `limit` terminates early — exactly what WDBench (output cap 100k) measures.
/// `limit=None` produces everything up to the cap.
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
    // Stop once this many rows exist: the LIMIT (clean stop) or cap+1 (then the
    // caller reports "too large"). Saturating against MAX.
    let stop = limit
        .map(|l| l.min(cap.saturating_add(1)))
        .unwrap_or(cap.saturating_add(1));

    let mut out = RowBlock::new(n_vars);
    if plan.steps.is_empty() {
        return Ok(out);
    }

    // Seed: materialize the most selective (first) pattern. The planner picks it
    // as the most selective -> small; a degenerate full scan is caught by the cap.
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
        // Single pattern: take rows directly up to `stop`.
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

/// Passes a partial row through the remaining plan steps (depth-first) and
/// appends complete rows to `out` until `stop` is reached.
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
    // Extensions of the partial row by ONE pattern (bounded by this node's
    // fan-out, not by the global intermediate set).
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
        // `tmp` is not mutated in the recursion -> pass the slice straight through.
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

/// Evaluates a pattern against the store and writes the (extended) rows
/// directly into `out` – without a temporary row `Vec` per result row.
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

/// Executes a graph pattern with a worst-case-optimal join.
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
    // Stop threshold: LIMIT (clean early stop) or cap+1 (then the caller reports
    // "too large"). Brakes the (parallel) recursion IMMEDIATELY rather than
    // after the fact -> no OOM.
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
        true, // first level in parallel
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
        // Exactly one bound predicate and two variables at subject/object
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
    // Heuristic: variables in order of first appearance.
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
    // Global abort once the cap is reached (by any branch).
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

/// Intersection of the candidates for `var_name` across all affected patterns.
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

/// Returns the sorted candidate slice for a variable in a pattern.
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

/// Leapfrog intersection of several sorted slices.
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

// Helper methods for PatternTerm
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

    /// Concrete u32 value, if a constant or an already-bound variable.
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
mod rowblock_tests {
    use super::*;

    #[test]
    fn push_and_iterate() {
        let mut b = RowBlock::new(2);
        b.push_row(&[1, 2]);
        b.push_row(&[3, 4]);
        assert_eq!(b.n_rows(), 2);
        assert_eq!(b.n_vars(), 2);
        assert_eq!(b.row(1), &[3, 4]);
        let all: Vec<&[u32]> = b.rows().collect();
        assert_eq!(all, vec![&[1u32, 2][..], &[3, 4][..]]);
    }

    #[test]
    fn push_concat_and_padded() {
        let mut b = RowBlock::new(3);
        b.push_row_concat(&[1], &[2, 3]); // prefix ++ suffix
        b.push_row_padded(&[9], UNBOUND); // prefix, rest UNBOUND
        assert_eq!(b.row(0), &[1, 2, 3]);
        assert_eq!(b.row(1), &[9, UNBOUND, UNBOUND]);
    }

    #[test]
    fn project_selects_columns() {
        let mut b = RowBlock::new(3);
        b.push_row(&[10, 20, 30]);
        b.push_row(&[40, 50, 60]);
        let p = b.project(&[2, 0]); // (o, s)
        assert_eq!(p.n_vars(), 2);
        assert_eq!(p.row(0), &[30, 10]);
        assert_eq!(p.row(1), &[60, 40]);
    }

    #[test]
    fn sort_distinct_dedups_and_orders() {
        let mut b = RowBlock::new(1);
        for v in [3u32, 1, 2, 1, 3] {
            b.push_row(&[v]);
        }
        b.sort_distinct();
        let got: Vec<u32> = b.rows().map(|r| r[0]).collect();
        assert_eq!(got, vec![1, 2, 3]);
    }

    #[test]
    fn dedup_preserves_first_seen_order() {
        let mut b = RowBlock::new(1);
        for v in [3u32, 1, 3, 2, 1] {
            b.push_row(&[v]);
        }
        b.dedup_preserving_order();
        let got: Vec<u32> = b.rows().map(|r| r[0]).collect();
        assert_eq!(got, vec![3, 1, 2]); // order of first occurrence
    }

    #[test]
    fn offset_limit_slices() {
        let mut b = RowBlock::new(1);
        for v in 0..10u32 {
            b.push_row(&[v]);
        }
        b.apply_offset_limit(3, Some(4));
        let got: Vec<u32> = b.rows().map(|r| r[0]).collect();
        assert_eq!(got, vec![3, 4, 5, 6]);
    }

    #[test]
    fn append_concatenates() {
        let mut a = RowBlock::new(1);
        a.push_row(&[1]);
        let mut b = RowBlock::new(1);
        b.push_row(&[2]);
        b.push_row(&[3]);
        a.append(&b);
        assert_eq!(a.n_rows(), 3);
        assert_eq!(a.row(2), &[3]);
    }

    #[test]
    fn zero_var_distinct_collapses() {
        // A 0-variable block (e.g. ASK/existence) -> at most one row.
        let mut b = RowBlock::new(0);
        b.push_row(&[]);
        b.push_row(&[]);
        b.sort_distinct();
        assert_eq!(b.n_rows(), 1);
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
            ("alice", "knows", "dave"), // no triangle
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
        // Each rotation of the triangle is a result: (a,b,c), (b,c,a), (c,a,b)
        assert_eq!(results.n_rows(), 3);
    }
}
