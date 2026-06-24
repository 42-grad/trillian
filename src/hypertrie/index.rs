use std::borrow::Cow;
use std::sync::Arc;

use memmap2::Mmap;
use rustc_hash::FxHashMap;

/// A `u32` array that either lives in RAM (`Owned`) or points **zero-copy**
/// into a memory-mapped snapshot file (`Mapped`). This lets the index be served
/// directly from the file on load (zero-copy, memory-mapped) without copying it
/// into RAM.
#[derive(Debug, Clone)]
pub enum U32Arena {
    Owned(Vec<u32>),
    Mapped {
        map: Arc<Mmap>,
        byte_offset: usize,
        len: usize,
    },
}

impl Default for U32Arena {
    fn default() -> Self {
        U32Arena::Owned(Vec::new())
    }
}

impl U32Arena {
    #[inline]
    pub fn as_slice(&self) -> &[u32] {
        match self {
            U32Arena::Owned(v) => v.as_slice(),
            U32Arena::Mapped {
                map,
                byte_offset,
                len,
            } => {
                let bytes = &map[*byte_offset..*byte_offset + *len * 4];
                bytemuck::cast_slice(bytes)
            }
        }
    }
}

/// Compact, **flat CSR arena** for a fixed triple permutation (e.g. SPO, POS,
/// OSP) plus a small **delta overlay** for incremental updates.
///
/// Motivation: the earlier `BTreeMap<u32, BTreeMap<u32, Vec<u32>>>` variant
/// produced millions of tiny heap allocations (one `Vec` per leaf + BTreeMap
/// nodes) and thus dominated RSS. The flat CSR base keeps all data in a few
/// large, contiguous vectors.
///
/// So updates still stay incremental and fast (flat CSR cannot cheaply insert
/// in the middle), a delta sits on top of it:
/// * `ins`: per `(first, second)` the added, sorted `third` values (disjoint
///   from the base).
/// * `del`: per `(first, second)` the deleted, sorted `third` values.
///
/// If the delta grows too large, it is folded into a new flat base via
/// [`compact`](Self::compact).
#[derive(Debug, Clone, Default)]
pub struct LayeredIndex {
    base: FlatCsr,
    ins: FxHashMap<(u32, u32), Vec<u32>>,
    del: FxHashMap<(u32, u32), Vec<u32>>,
    len: usize,
}

/// Immutable, three-level flat CSR structure. The five arrays live either in
/// RAM or zero-copy in an mmap snapshot ([`U32Arena`]).
#[derive(Debug, Clone, Default)]
pub struct FlatCsr {
    keys: U32Arena,    // sorted, distinct first values
    key_off: U32Arena, // keys.len()+1; range in l1
    l1: U32Arena,      // per first: sorted distinct second values
    l1_off: U32Arena,  // l1.len()+1; range in vals
    vals: U32Arena,    // per (first, second): sorted distinct thirds
}

impl FlatCsr {
    /// Builds the flat CSR from an (unsorted) triple list.
    fn build(triples: &[(u32, u32, u32)]) -> Self {
        let mut sorted = triples.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        let mut keys = Vec::new();
        let mut key_off = vec![0u32];
        let mut l1 = Vec::new();
        let mut l1_off = vec![0u32];
        let mut vals = Vec::new();

        let mut i = 0;
        while i < sorted.len() {
            let first = sorted[i].0;
            keys.push(first);
            while i < sorted.len() && sorted[i].0 == first {
                let second = sorted[i].1;
                l1.push(second);
                while i < sorted.len() && sorted[i].0 == first && sorted[i].1 == second {
                    vals.push(sorted[i].2);
                    i += 1;
                }
                l1_off.push(vals.len() as u32);
            }
            key_off.push(l1.len() as u32);
        }

        FlatCsr {
            keys: U32Arena::Owned(keys),
            key_off: U32Arena::Owned(key_off),
            l1: U32Arena::Owned(l1),
            l1_off: U32Arena::Owned(l1_off),
            vals: U32Arena::Owned(vals),
        }
    }

    /// Constructs a FlatCsr from five arenas (e.g. mmap slices).
    pub fn from_arenas(
        keys: U32Arena,
        key_off: U32Arena,
        l1: U32Arena,
        l1_off: U32Arena,
        vals: U32Arena,
    ) -> Self {
        FlatCsr {
            keys,
            key_off,
            l1,
            l1_off,
            vals,
        }
    }

    /// The five arrays as slices (for serialization).
    pub fn arrays(&self) -> [&[u32]; 5] {
        [
            self.keys.as_slice(),
            self.key_off.as_slice(),
            self.l1.as_slice(),
            self.l1_off.as_slice(),
            self.vals.as_slice(),
        ]
    }

    #[inline]
    fn keys(&self) -> &[u32] {
        self.keys.as_slice()
    }
    #[inline]
    fn key_off(&self) -> &[u32] {
        self.key_off.as_slice()
    }
    #[inline]
    fn l1(&self) -> &[u32] {
        self.l1.as_slice()
    }
    #[inline]
    fn l1_off(&self) -> &[u32] {
        self.l1_off.as_slice()
    }
    #[inline]
    fn vals(&self) -> &[u32] {
        self.vals.as_slice()
    }

    /// Absolute l1 index for `(first, second)`, if present.
    #[inline]
    fn leaf_index(&self, first: u32, second: u32) -> Option<usize> {
        let keys = self.keys();
        let i = keys.binary_search(&first).ok()?;
        let key_off = self.key_off();
        let start = key_off[i] as usize;
        let end = key_off[i + 1] as usize;
        self.l1()[start..end]
            .binary_search(&second)
            .ok()
            .map(|p| p + start)
    }

    #[inline]
    fn query_two(&self, first: u32, second: u32) -> &[u32] {
        match self.leaf_index(first, second) {
            Some(j) => {
                let l1_off = self.l1_off();
                let s = l1_off[j] as usize;
                let e = l1_off[j + 1] as usize;
                &self.vals()[s..e]
            }
            None => &[],
        }
    }

    #[inline]
    fn contains(&self, first: u32, second: u32, third: u32) -> bool {
        self.query_two(first, second).binary_search(&third).is_ok()
    }

    /// Distinct, sorted `second` values under `first` as a contiguous slice
    /// (the L1 level). Empty if `first` is absent. Zero-copy.
    #[inline]
    fn seconds_of(&self, first: u32) -> &[u32] {
        let keys = self.keys();
        let Ok(i) = keys.binary_search(&first) else {
            return &[];
        };
        let key_off = self.key_off();
        &self.l1()[key_off[i] as usize..key_off[i + 1] as usize]
    }

    /// Number of `third` values under `first` across all `second` (O(log n)
    /// lookup, O(1) sum via CSR offsets). 0 if `first` is absent.
    #[inline]
    fn count_one(&self, first: u32) -> usize {
        let keys = self.keys();
        let Ok(i) = keys.binary_search(&first) else {
            return 0;
        };
        let key_off = self.key_off();
        let l1_off = self.l1_off();
        let l1_start = key_off[i] as usize;
        let l1_end = key_off[i + 1] as usize;
        l1_off[l1_end] as usize - l1_off[l1_start] as usize
    }

    fn all_triples(&self) -> Vec<(u32, u32, u32)> {
        let keys = self.keys();
        let key_off = self.key_off();
        let l1 = self.l1();
        let l1_off = self.l1_off();
        let vals = self.vals();
        let mut out = Vec::with_capacity(vals.len());
        for (i, &first) in keys.iter().enumerate() {
            let ls = key_off[i] as usize;
            let le = key_off[i + 1] as usize;
            for j in ls..le {
                let second = l1[j];
                let vs = l1_off[j] as usize;
                let ve = l1_off[j + 1] as usize;
                for &third in &vals[vs..ve] {
                    out.push((first, second, third));
                }
            }
        }
        out
    }

    #[inline]
    fn len(&self) -> usize {
        self.vals().len()
    }
}

impl LayeredIndex {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds the index (flat base, empty delta) from a triple list.
    pub fn build(triples: &[(u32, u32, u32)]) -> Self {
        let base = FlatCsr::build(triples);
        let len = base.len();
        Self {
            base,
            ins: FxHashMap::default(),
            del: FxHashMap::default(),
            len,
        }
    }

    /// Constructs the index from a (e.g. mmap-mapped) base, empty delta.
    pub fn from_base(base: FlatCsr) -> Self {
        let len = base.len();
        Self {
            base,
            ins: FxHashMap::default(),
            del: FxHashMap::default(),
            len,
        }
    }

    /// Returns the flat base (only meaningful after [`compact`](Self::compact),
    /// when the delta is empty) – for snapshot serialization.
    pub fn base(&self) -> &FlatCsr {
        &self.base
    }

    /// Whether the delta is empty (base = full contents).
    pub fn delta_is_empty(&self) -> bool {
        self.ins.is_empty() && self.del.is_empty()
    }

    /// Number of delta entries (for the compaction heuristic).
    #[inline]
    fn delta_count(&self) -> usize {
        self.ins.values().map(|v| v.len()).sum::<usize>()
            + self.del.values().map(|v| v.len()).sum::<usize>()
    }

    /// Folds the delta into a fresh flat base when it has grown too large
    /// relative to the base. Keeps memory bounded.
    fn maybe_compact(&mut self) {
        let delta = self.delta_count();
        if delta > 1024 && delta * 4 > self.base.len() {
            self.compact();
        }
    }

    /// Folds the entire delta into a new flat base.
    pub fn compact(&mut self) {
        if self.ins.is_empty() && self.del.is_empty() {
            return;
        }
        let triples = self.all_triples();
        self.base = FlatCsr::build(&triples);
        self.ins.clear();
        self.del.clear();
        self.len = self.base.len();
    }

    /// Inserts `(first, second, third)`. Returns `true` if new.
    pub fn insert(&mut self, first: u32, second: u32, third: u32) -> bool {
        if self.contains(first, second, third) {
            return false;
        }
        // If marked as deleted: undo the deletion.
        if remove_from_leaf(&mut self.del, first, second, third) {
            // was in the base, now visible again
        } else {
            insert_into_leaf(&mut self.ins, first, second, third);
        }
        self.len += 1;
        self.maybe_compact();
        true
    }

    /// Removes `(first, second, third)`. Returns `true` if present.
    pub fn delete(&mut self, first: u32, second: u32, third: u32) -> bool {
        if !self.contains(first, second, third) {
            return false;
        }
        // If present as a delta insert: remove it there, otherwise record in del.
        if remove_from_leaf(&mut self.ins, first, second, third) {
            // was a delta insert
        } else {
            insert_into_leaf(&mut self.del, first, second, third);
        }
        self.len -= 1;
        self.maybe_compact();
        true
    }

    /// Exact existence check.
    pub fn contains(&self, first: u32, second: u32, third: u32) -> bool {
        if leaf_has(&self.del, first, second, third) {
            return false;
        }
        if leaf_has(&self.ins, first, second, third) {
            return true;
        }
        self.base.contains(first, second, third)
    }

    /// Query `(first, second, ?third)` as a sorted slice.
    ///
    /// With no delta hit at this position, the base slice is returned
    /// **borrowed** (no allocation); otherwise the merged set
    /// `(base ∪ ins) \ del` as `Cow::Owned`.
    pub fn query_two(&self, first: u32, second: u32) -> Cow<'_, [u32]> {
        let base = self.base.query_two(first, second);
        let ins = self.ins.get(&(first, second));
        let del = self.del.get(&(first, second));
        if ins.is_none() && del.is_none() {
            return Cow::Borrowed(base);
        }
        let del = del.map(|v| v.as_slice()).unwrap_or(&[]);
        let ins = ins.map(|v| v.as_slice()).unwrap_or(&[]);
        Cow::Owned(merge_union_minus(base, ins, del))
    }

    /// Distinct `first` values (base keys + delta). For property paths (start
    /// candidates) and enumerations.
    pub fn first_keys(&self) -> Vec<u32> {
        let mut ks: Vec<u32> = self.base.keys().to_vec();
        if !self.ins.is_empty() {
            for &(f, _) in self.ins.keys() {
                ks.push(f);
            }
            ks.sort_unstable();
            ks.dedup();
        }
        ks
    }

    /// Distinct, sorted `second` values under `first` (the L1 level). In the
    /// delta-free case a zero-copy borrow of the base; merged when there is a
    /// delta. Replaces precomputed predicate lists (e.g. `objects_with_predicate`
    /// over POS), saving a full owned copy of the data.
    pub fn seconds_of(&self, first: u32) -> Cow<'_, [u32]> {
        let base = self.base.seconds_of(first);
        if self.ins.is_empty() && self.del.is_empty() {
            return Cow::Borrowed(base);
        }
        // With a delta: distinct seconds from base + inserts, minus fully
        // deleted ones (seconds whose every third was removed) – checked
        // conservatively via query_two.
        let mut out: Vec<u32> = base.to_vec();
        for &(f, s) in self.ins.keys() {
            if f == first {
                out.push(s);
            }
        }
        out.sort_unstable();
        out.dedup();
        out.retain(|&s| !self.query_two(first, s).is_empty());
        Cow::Owned(out)
    }

    /// Number of `third` values under `(first, second)` without materialization
    /// in the delta-free case (then just the base slice length).
    #[inline]
    pub fn count_two(&self, first: u32, second: u32) -> usize {
        self.query_two(first, second).len()
    }

    /// Number of triples under `first` (base + delta inserts; deletions are not
    /// subtracted for this **heuristic**). Exact in the delta-free case.
    pub fn count_one(&self, first: u32) -> usize {
        let mut c = self.base.count_one(first);
        if !self.ins.is_empty() {
            for (&(f, _s), thirds) in &self.ins {
                if f == first {
                    c += thirds.len();
                }
            }
        }
        c
    }

    /// Query `(first, ?second, ?third)`: materialized `(second, third)` pairs.
    pub fn query_one_pairs(&self, first: u32) -> Vec<(u32, u32)> {
        // Collect distinct second values under first (base + delta inserts).
        let mut seconds: Vec<u32> = Vec::new();
        let base_keys = self.base.keys();
        if let Ok(i) = base_keys.binary_search(&first) {
            let key_off = self.base.key_off();
            let ls = key_off[i] as usize;
            let le = key_off[i + 1] as usize;
            seconds.extend_from_slice(&self.base.l1()[ls..le]);
        }
        for &(f, s) in self.ins.keys() {
            if f == first {
                seconds.push(s);
            }
        }
        seconds.sort_unstable();
        seconds.dedup();

        let mut out = Vec::new();
        for s in seconds {
            for &t in self.query_two(first, s).iter() {
                out.push((s, t));
            }
        }
        out
    }

    /// All stored triples in permutation order.
    pub fn all_triples(&self) -> Vec<(u32, u32, u32)> {
        if self.ins.is_empty() && self.del.is_empty() {
            return self.base.all_triples();
        }
        let mut out = self.base.all_triples();
        // Remove deletions.
        if !self.del.is_empty() {
            out.retain(|&(f, s, t)| !leaf_has(&self.del, f, s, t));
        }
        // Add delta inserts.
        for (&(f, s), thirds) in &self.ins {
            for &t in thirds {
                out.push((f, s, t));
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Logical byte size (base arrays + delta) – for the memory report.
    pub fn heap_bytes(&self) -> usize {
        let base: usize = self.base.arrays().iter().map(|a| a.len() * 4).sum();
        let delta: usize = self
            .ins
            .values()
            .chain(self.del.values())
            .map(|v| v.len() * 4 + 24)
            .sum();
        base + delta
    }
}

// --- Delta-leaf helpers (sorted Vecs per (first, second)) ---

#[inline]
fn leaf_has(map: &FxHashMap<(u32, u32), Vec<u32>>, first: u32, second: u32, third: u32) -> bool {
    map.get(&(first, second))
        .is_some_and(|v| v.binary_search(&third).is_ok())
}

#[inline]
fn insert_into_leaf(
    map: &mut FxHashMap<(u32, u32), Vec<u32>>,
    first: u32,
    second: u32,
    third: u32,
) {
    let leaf = map.entry((first, second)).or_default();
    if let Err(pos) = leaf.binary_search(&third) {
        leaf.insert(pos, third);
    }
}

/// Removes `third` from the delta leaf; returns `true` if it was there.
#[inline]
fn remove_from_leaf(
    map: &mut FxHashMap<(u32, u32), Vec<u32>>,
    first: u32,
    second: u32,
    third: u32,
) -> bool {
    if let Some(leaf) = map.get_mut(&(first, second))
        && let Ok(pos) = leaf.binary_search(&third)
    {
        leaf.remove(pos);
        if leaf.is_empty() {
            map.remove(&(first, second));
        }
        return true;
    }
    false
}

/// `(base ∪ ins) \ del` for three sorted, distinct slices.
/// `base` and `ins` are disjoint; `del ⊆ base ∪ ins`.
fn merge_union_minus(base: &[u32], ins: &[u32], del: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(base.len() + ins.len());
    let (mut i, mut j) = (0, 0);
    while i < base.len() || j < ins.len() {
        let v = match (base.get(i), ins.get(j)) {
            (Some(&b), Some(&n)) => {
                if b <= n {
                    i += 1;
                    b
                } else {
                    j += 1;
                    n
                }
            }
            (Some(&b), None) => {
                i += 1;
                b
            }
            (None, Some(&n)) => {
                j += 1;
                n
            }
            (None, None) => unreachable!(),
        };
        if del.binary_search(&v).is_err() {
            out.push(v);
        }
    }
    out
}

/// Intersection of two sorted u32 slices via a classic merge.
pub fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(c: Cow<[u32]>) -> Vec<u32> {
        c.into_owned()
    }

    #[test]
    fn build_and_query() {
        let idx = LayeredIndex::build(&[(1, 2, 3), (1, 2, 4), (1, 5, 6), (7, 8, 9)]);
        assert_eq!(idx.len(), 4);
        assert_eq!(collect(idx.query_two(1, 2)), vec![3, 4]);
        assert_eq!(collect(idx.query_two(1, 5)), vec![6]);
        assert!(idx.query_two(9, 9).is_empty());
        assert!(idx.contains(1, 2, 3));
        assert!(!idx.contains(1, 2, 99));
    }

    #[test]
    fn counts_and_keys() {
        let idx = LayeredIndex::build(&[(1, 2, 3), (1, 2, 4), (1, 5, 6), (7, 8, 9)]);
        // count_one(first) = total thirds under first, over all seconds
        assert_eq!(idx.count_one(1), 3); // (1,2,3),(1,2,4),(1,5,6)
        assert_eq!(idx.count_one(7), 1);
        assert_eq!(idx.count_one(42), 0);
        // count_two(first, second) = thirds under (first, second)
        assert_eq!(idx.count_two(1, 2), 2);
        assert_eq!(idx.count_two(1, 5), 1);
        assert_eq!(idx.count_two(1, 9), 0);
        // first_keys = distinct, sorted firsts
        assert_eq!(idx.first_keys(), vec![1, 7]);
        // seconds_of = distinct, sorted seconds under a first (zero-copy base)
        assert_eq!(collect(idx.seconds_of(1)), vec![2, 5]);
        assert!(idx.seconds_of(42).is_empty());
    }

    #[test]
    fn seconds_of_merges_delta() {
        let mut idx = LayeredIndex::build(&[(1, 2, 3)]);
        idx.insert(1, 8, 9); // new second 8 under first 1 (delta)
        let mut secs = collect(idx.seconds_of(1));
        secs.sort_unstable();
        assert_eq!(secs, vec![2, 8]);
        // after deleting the only third under (1,8), second 8 disappears
        idx.delete(1, 8, 9);
        assert_eq!(collect(idx.seconds_of(1)), vec![2]);
    }

    #[test]
    fn delta_insert_delete_roundtrip() {
        let mut idx = LayeredIndex::build(&[(1, 2, 3), (1, 2, 5)]);
        // Insert into existing group -> merge.
        assert!(idx.insert(1, 2, 4));
        assert_eq!(collect(idx.query_two(1, 2)), vec![3, 4, 5]);
        assert!(!idx.insert(1, 2, 4)); // duplicate
        assert_eq!(idx.len(), 3);

        // Insert into new group.
        assert!(idx.insert(9, 9, 9));
        assert_eq!(collect(idx.query_two(9, 9)), vec![9]);

        // Delete a base value.
        assert!(idx.delete(1, 2, 3));
        assert_eq!(collect(idx.query_two(1, 2)), vec![4, 5]);
        assert!(!idx.contains(1, 2, 3));

        // Delete a delta insert.
        assert!(idx.delete(1, 2, 4));
        assert_eq!(collect(idx.query_two(1, 2)), vec![5]);

        // Re-insert a deleted base value.
        assert!(idx.insert(1, 2, 3));
        assert_eq!(collect(idx.query_two(1, 2)), vec![3, 5]);

        assert!(!idx.delete(1, 2, 999)); // not present
    }

    #[test]
    fn all_triples_with_delta() {
        let mut idx = LayeredIndex::build(&[(1, 1, 1), (2, 2, 2)]);
        idx.insert(3, 3, 3);
        idx.delete(1, 1, 1);
        let mut all = idx.all_triples();
        all.sort_unstable();
        assert_eq!(all, vec![(2, 2, 2), (3, 3, 3)]);
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn compaction_preserves_contents() {
        let mut idx = LayeredIndex::build(&[(1, 1, 1)]);
        // Force enough inserts to trigger maybe_compact.
        for t in 0..5000u32 {
            idx.insert(2, 2, t);
        }
        // After (any) compaction all data must still be correct.
        idx.compact();
        assert!(idx.ins.is_empty() && idx.del.is_empty());
        assert_eq!(idx.len(), 5001);
        assert!(idx.contains(1, 1, 1));
        assert_eq!(idx.query_two(2, 2).len(), 5000);
    }

    #[test]
    fn query_one_pairs_merges_delta() {
        let mut idx = LayeredIndex::build(&[(1, 2, 3)]);
        idx.insert(1, 4, 5);
        let mut pairs = idx.query_one_pairs(1);
        pairs.sort_unstable();
        assert_eq!(pairs, vec![(2, 3), (4, 5)]);
    }
}
