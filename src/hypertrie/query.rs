use std::borrow::Cow;

use rustc_hash::FxHashMap;

use super::dictionary::{Dictionary, TermType};
use super::export::ParsedTriple;
use super::index::{FlatCsr, LayeredIndex, U32Arena, intersect_sorted};
use super::stats::CardEstimator;

/// Signature at the start of every snapshot file. The last digit is the format
/// generation; `SNAP_VERSION` versions the layout beneath it. On load both are
/// checked so an old/foreign format yields an error instead of silently mapping
/// wrong data.
const SNAP_MAGIC: &[u8; 8] = b"TTRSNAP1";
const SNAP_VERSION: u32 = 5; // v5: mmap dictionary with u64 offsets (>4 GB blob)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Var {
    S,
    P,
    O,
}

#[derive(Debug, Clone, Copy)]
pub enum Term {
    Bound(u32),
    Wildcard,
}

#[derive(Debug, Clone)]
pub enum QueryResult<'a> {
    /// No match.
    Empty,
    /// `(S, P, O)` – exact existence check.
    Exact(bool),
    /// One free variable; the values are returned borrowed (no delta) or as a
    /// merged set (with delta).
    Single(Var, Cow<'a, [u32]>),
    /// Two free variables; pairs are materialized.
    Double(Var, Var, Vec<(u32, u32)>),
    /// Three free variables; all triples materialized.
    All(Vec<(u32, u32, u32)>),
}

/// Aggregate storage of all three permutation indexes, the dictionary, stats,
/// and the slim predicate key lists for WCOJ.
///
/// The indexes are the sole source of truth – there is no separate triple list
/// any more. This makes `insert_triple`/`delete_triple`/`apply_updates` true
/// **incremental** in-place operations without a full rebuild.
///
/// Stage 1 of the hypertrie rework: the earlier forward/reverse CSR relations
/// per predicate (two full data copies) are removed. WCOJ takes
/// `objects_for`/`subjects_for` directly from the permutations (SPO/POS).
/// `objects_with_predicate` comes zero-copy from the POS L1 level; only
/// `pred_subjects` (distinct subjects per p) is still held – the one direction
/// that no index provides as a contiguous slice.
pub struct TripleStore {
    pub dict: Dictionary,
    /// p -> sorted, distinct subjects with predicate p (for WCOJ candidates).
    pred_subjects: FxHashMap<u32, Vec<u32>>,
    spo: LayeredIndex, // order: S, P, O
    pos: LayeredIndex, // order: P, O, S
    osp: LayeredIndex, // order: O, S, P
}

impl TripleStore {
    pub fn new() -> Self {
        Self {
            dict: Dictionary::new(),
            pred_subjects: FxHashMap::default(),
            spo: LayeredIndex::empty(),
            pos: LayeredIndex::empty(),
            osp: LayeredIndex::empty(),
        }
    }

    /// Ingest from parsed RDF triples with term types.
    pub fn ingest(&mut self, triples: &[ParsedTriple]) {
        let mut id_triples = Vec::with_capacity(triples.len());
        for t in triples {
            let sid = self
                .dict
                .insert_with_type(&t.subject.value, t.subject.typ.clone());
            let pid = self
                .dict
                .insert_with_type(&t.predicate.value, t.predicate.typ.clone());
            let oid = self
                .dict
                .insert_with_type(&t.object.value, t.object.typ.clone());
            id_triples.push((sid, pid, oid));
        }
        self.build_indexes(id_triples);
    }

    /// Like [`ingest`](Self::ingest), but takes the parsed triples by value and
    /// **frees the string buffer before building the indexes**.
    pub fn ingest_owned(&mut self, triples: Vec<ParsedTriple>) {
        let mut id_triples = Vec::with_capacity(triples.len());
        for t in &triples {
            let sid = self
                .dict
                .insert_with_type(&t.subject.value, t.subject.typ.clone());
            let pid = self
                .dict
                .insert_with_type(&t.predicate.value, t.predicate.typ.clone());
            let oid = self
                .dict
                .insert_with_type(&t.object.value, t.object.typ.clone());
            id_triples.push((sid, pid, oid));
        }
        drop(triples); // free the parse buffer (strings)
        self.build_indexes(id_triples);
    }

    /// **Streaming** load of an N-Triples file: each line is parsed individually
    /// and immediately mapped into the dictionary (ID triple), without ever
    /// holding the entire `ParsedTriple` buffer (~3M strings) in memory.
    ///
    /// This lowers **both** the peak RSS (no parse buffer during load) **and**
    /// the resident RSS. Returns the number of triples loaded.
    pub fn ingest_ntriples_file(&mut self, path: &str) -> std::io::Result<usize> {
        use std::io::BufRead;
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        let mut id_triples: Vec<(u32, u32, u32)> = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(t) = super::export::parse_triple_line(line) {
                let sid = self.dict.insert_with_type(&t.subject.value, t.subject.typ);
                let pid = self
                    .dict
                    .insert_with_type(&t.predicate.value, t.predicate.typ);
                let oid = self.dict.insert_with_type(&t.object.value, t.object.typ);
                id_triples.push((sid, pid, oid));
            }
        }
        let n = id_triples.len();
        self.build_indexes(id_triples);
        Ok(n)
    }

    /// Ingest from string triples (backward compatibility; everything as IRI).
    pub fn ingest_str_triples(&mut self, triples: &[(&str, &str, &str)]) {
        let parsed: Vec<ParsedTriple> = triples
            .iter()
            .map(|(s, p, o)| ParsedTriple {
                subject: super::export::ParsedTerm {
                    value: s.to_string(),
                    typ: TermType::Iri,
                },
                predicate: super::export::ParsedTerm {
                    value: p.to_string(),
                    typ: TermType::Iri,
                },
                object: super::export::ParsedTerm {
                    value: o.to_string(),
                    typ: TermType::Iri,
                },
            })
            .collect();
        self.ingest(&parsed);
    }

    /// Direct ingest from already-mapped IDs (e.g. for benchmarks).
    pub fn ingest_id_triples(&mut self, triples: Vec<(u32, u32, u32)>) {
        self.build_indexes(triples);
    }

    /// Inserts a single triple **incrementally** (no rebuild).
    pub fn insert_triple(&mut self, s: u32, p: u32, o: u32) {
        self.add_one(s, p, o);
    }

    /// Removes a single triple **incrementally** (no rebuild).
    pub fn delete_triple(&mut self, s: u32, p: u32, o: u32) {
        self.remove_one(s, p, o);
    }

    /// Applies several inserts and deletes incrementally. Deletes first.
    /// Each operation touches only the affected index nodes – there is no full
    /// rebuild any more.
    pub fn apply_updates(&mut self, inserts: &[(u32, u32, u32)], deletes: &[(u32, u32, u32)]) {
        for &(s, p, o) in deletes {
            self.remove_one(s, p, o);
        }
        for &(s, p, o) in inserts {
            self.add_one(s, p, o);
        }
    }

    /// Incremental insert into all three permutations, the predicate key lists,
    /// and the statistics. Returns `true` if the triple was new.
    fn add_one(&mut self, s: u32, p: u32, o: u32) -> bool {
        let is_new = self.spo.insert(s, p, o);
        if is_new {
            self.pos.insert(p, o, s);
            self.osp.insert(o, s, p);
            sorted_insert(self.pred_subjects.entry(p).or_default(), s);
            // pred_objects dropped: objects_with_predicate comes zero-copy from
            // the POS L1 level (seconds_of).
        }
        is_new
    }

    /// Incremental removal from all structures. A subject/object ID drops out of
    /// the predicate key list only when it has no further triple under p.
    /// Returns `true` if it was present.
    fn remove_one(&mut self, s: u32, p: u32, o: u32) -> bool {
        let existed = self.spo.delete(s, p, o);
        if existed {
            self.pos.delete(p, o, s);
            self.osp.delete(o, s, p);

            // s loses its entry in pred_subjects[p] only when (s,p) has no
            // object left.
            if self.spo.query_two(s, p).is_empty()
                && let Some(subs) = self.pred_subjects.get_mut(&p)
            {
                sorted_remove(subs, s);
                if subs.is_empty() {
                    self.pred_subjects.remove(&p);
                }
            }
        }
        existed
    }

    // --- WCOJ/slice accessors (replace the earlier PredicateRelation APIs) ---

    /// Objects of (s, p) as a sorted slice – directly from the SPO index.
    #[inline]
    pub fn objects_of(&self, s: u32, p: u32) -> Cow<'_, [u32]> {
        self.spo.query_two(s, p)
    }

    /// Subjects of (p, o) as a sorted slice – directly from the POS index.
    #[inline]
    pub fn subjects_of(&self, p: u32, o: u32) -> Cow<'_, [u32]> {
        self.pos.query_two(p, o)
    }

    /// Sorted, distinct subjects with predicate p.
    #[inline]
    pub fn subjects_with_predicate(&self, p: u32) -> &[u32] {
        self.pred_subjects.get(&p).map_or(&[], |v| v.as_slice())
    }

    /// Sorted, distinct objects with predicate p – zero-copy from the POS L1
    /// level (no dedicated storage any more).
    #[inline]
    pub fn objects_with_predicate(&self, p: u32) -> Cow<'_, [u32]> {
        self.pos.seconds_of(p)
    }

    /// Whether predicate p occurs in the store (for WCOJ applicability).
    #[inline]
    pub fn has_predicate(&self, p: u32) -> bool {
        self.pred_subjects.contains_key(&p)
    }

    // --- Property-path accessors -----------------------------------------

    /// All `(p, o)` pairs of subject s (for negated property sets, forward).
    #[inline]
    pub fn po_pairs_of(&self, s: u32) -> Vec<(u32, u32)> {
        self.spo.query_one_pairs(s)
    }

    /// All `(s, p)` pairs of object o (for negated property sets, backward).
    #[inline]
    pub fn sp_pairs_of(&self, o: u32) -> Vec<(u32, u32)> {
        self.osp.query_one_pairs(o)
    }

    /// Distinct subjects (SPO keys) – start candidates for paths.
    #[inline]
    pub fn distinct_subjects(&self) -> Vec<u32> {
        self.spo.first_keys()
    }

    /// Distinct objects (OSP keys).
    #[inline]
    pub fn distinct_objects(&self) -> Vec<u32> {
        self.osp.first_keys()
    }

    /// Writes the entire store losslessly as an N-Triples file.
    ///
    /// Term types (IRI, literal with datatype/language tag) are preserved, so
    /// `parse_ntriples` + `ingest` reconstructs the store exactly. Serves as a
    /// simple, standard-compliant persistence layer.
    pub fn dump_ntriples(&self, path: &str) -> std::io::Result<()> {
        use std::io::Write;
        let file = std::fs::File::create(path)?;
        let mut writer = std::io::BufWriter::new(file);
        for (s, p, o) in self.spo.all_triples() {
            let line = format!(
                "{} {} {} .",
                self.serialize_id(s),
                self.serialize_id(p),
                self.serialize_id(o)
            );
            writeln!(writer, "{}", line)?;
        }
        writer.flush()
    }

    fn serialize_id(&self, id: u32) -> String {
        let value = self
            .dict
            .resolve(id)
            .unwrap_or(std::borrow::Cow::Borrowed(""));
        let typ = self
            .dict
            .resolve_type(id)
            .unwrap_or(super::dictionary::TermType::Iri);
        super::export::serialize_term(&value, &typ)
    }

    /// Rebuilds all indexes from a triple list (bulk load).
    ///
    /// Used only at ingest; updates afterwards run incrementally via
    /// [`add_one`](Self::add_one)/[`remove_one`](Self::remove_one).
    fn build_indexes(&mut self, triples: Vec<(u32, u32, u32)>) {
        // SPO directly; POS/OSP as permutations. `LayeredIndex::build` sorts
        // and deduplicates internally.
        self.spo = LayeredIndex::build(&triples);
        let pos: Vec<(u32, u32, u32)> = triples.iter().map(|t| (t.1, t.2, t.0)).collect();
        self.pos = LayeredIndex::build(&pos);
        let osp: Vec<(u32, u32, u32)> = triples.iter().map(|t| (t.2, t.0, t.1)).collect();
        self.osp = LayeredIndex::build(&osp);

        self.rebuild_aux();
    }

    /// Derives the predicate key lists from the (already built/mapped)
    /// permutations. Used after bulk load **and** after loading an mmap
    /// snapshot. Cardinalities come on demand from the index ([`CardEstimator`]),
    /// hence no precomputed stats maps any more.
    fn rebuild_aux(&mut self) {
        // Derive distinct subjects per predicate in O(n) – the sort order allows
        // last()-deduplication. (Objects: on demand from POS L1.)
        self.pred_subjects.clear();
        // SPO is sorted by (s,p,o) -> per predicate the s values are monotonic.
        for (s, p, _o) in self.spo.all_triples() {
            let subs = self.pred_subjects.entry(p).or_default();
            if subs.last() != Some(&s) {
                subs.push(s);
            }
        }
        // pred_objects dropped: objects_with_predicate comes zero-copy from POS.
    }

    /// Prints a logical memory breakdown (components in MB).
    /// Logical estimate – the real RSS additionally includes allocator overhead.
    pub fn memory_report(&self) {
        let mb = |b: usize| b as f64 / 1024.0 / 1024.0;
        let perm = self.spo.heap_bytes() + self.pos.heap_bytes() + self.osp.heap_bytes();
        let dict = self.dict.approx_bytes();
        let pred: usize = self
            .pred_subjects
            .values()
            .map(|v| v.len() * 4)
            .sum::<usize>();
        let total = perm + dict + pred;
        println!(
            "=== Memory report (logical, {} triples) ===",
            self.triple_count()
        );
        println!("  3 permutations (SPO/POS/OSP):    {:.1} MB", mb(perm));
        println!("  Dictionary (interned + types):   {:.1} MB", mb(dict));
        println!("  Predicate subjects (S only):     {:.1} MB", mb(pred));
        println!("  Stats maps:                      0.0 MB (on demand from index)");
        println!("  Total (logical):                 {:.1} MB", mb(total));
        println!(
            "  Bytes/triple (logical):          {:.0} B",
            total as f64 / self.triple_count().max(1) as f64
        );
    }

    /// Compacts the deltas of all three permutations into the flat bases.
    pub fn compact_all(&mut self) {
        self.spo.compact();
        self.pos.compact();
        self.osp.compact();
    }

    /// Writes a binary snapshot (dictionary + the 3 flat CSR indexes).
    ///
    /// The index arrays lie 4-byte aligned back to back, so they can be
    /// zero-copy memory-mapped on load.
    pub fn save_snapshot(&mut self, path: &str) -> std::io::Result<()> {
        self.compact_all();

        let perms = [self.spo.base(), self.pos.base(), self.osp.base()];
        let mut arrays: Vec<&[u32]> = Vec::with_capacity(15);
        for csr in perms {
            for a in csr.arrays() {
                arrays.push(a);
            }
        }

        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(SNAP_MAGIC);
        buf.extend_from_slice(&SNAP_VERSION.to_le_bytes()); // version
        buf.extend_from_slice(&(self.dict.len() as u32).to_le_bytes());
        let arrays_off_pos = buf.len();
        buf.extend_from_slice(&0u64.to_le_bytes()); // arrays_offset (placeholder)
        let dict_off_pos = buf.len();
        buf.extend_from_slice(&0u64.to_le_bytes()); // dict_offset (placeholder)
        for a in &arrays {
            buf.extend_from_slice(&(a.len() as u32).to_le_bytes());
        }
        while !buf.len().is_multiple_of(8) {
            buf.push(0);
        }
        let arrays_off = buf.len() as u64;
        for a in &arrays {
            buf.extend_from_slice(bytemuck::cast_slice::<u32, u8>(a));
        }
        while !buf.len().is_multiple_of(8) {
            buf.push(0); // start the dictionary section 8-byte aligned (u64 offsets)
        }
        let dict_off = buf.len() as u64;
        self.dict.serialize_into(&mut buf);

        buf[arrays_off_pos..arrays_off_pos + 8].copy_from_slice(&arrays_off.to_le_bytes());
        buf[dict_off_pos..dict_off_pos + 8].copy_from_slice(&dict_off.to_le_bytes());

        std::fs::write(path, buf)
    }

    /// Loads a snapshot via `mmap`: the index arrays are mapped **zero-copy**
    /// into the file, the dictionary is read through the mmap base; statistics
    /// and predicate lists are derived.
    pub fn load_snapshot(path: &str) -> std::io::Result<TripleStore> {
        let file = std::fs::File::open(path)?;
        // SAFETY: read-only snapshot; the file is not modified externally.
        let map = std::sync::Arc::new(unsafe { memmap2::Mmap::map(&file)? });
        let b: &[u8] = &map;

        let rd_u32 = |b: &[u8], p: usize| u32::from_le_bytes(b[p..p + 4].try_into().unwrap());
        let rd_u64 = |b: &[u8], p: usize| u64::from_le_bytes(b[p..p + 8].try_into().unwrap());

        let bad = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);
        // Header (magic + version + length table) must be fully present.
        if b.len() < 32 {
            return Err(bad("snapshot too short (header incomplete)".into()));
        }
        if &b[0..8] != SNAP_MAGIC {
            return Err(bad(format!(
                "invalid snapshot signature (expected {:?})",
                std::str::from_utf8(SNAP_MAGIC).unwrap_or("?")
            )));
        }
        let version = rd_u32(b, 8);
        if version != SNAP_VERSION {
            return Err(bad(format!(
                "incompatible snapshot version {version} (supported: {SNAP_VERSION})"
            )));
        }
        let arrays_off = rd_u64(b, 16) as usize;
        let dict_off = rd_u64(b, 24) as usize;
        let mut p = 32;
        let mut lens = [0usize; 15];
        for l in &mut lens {
            *l = rd_u32(b, p) as usize;
            p += 4;
        }

        let mut byte_off = arrays_off;
        let mut arenas: Vec<U32Arena> = Vec::with_capacity(15);
        for &len in &lens {
            arenas.push(U32Arena::Mapped {
                map: map.clone(),
                byte_offset: byte_off,
                len,
            });
            byte_off += len * 4;
        }

        let mut it = arenas.into_iter();
        let take5 = |it: &mut std::vec::IntoIter<U32Arena>| {
            FlatCsr::from_arenas(
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
                it.next().unwrap(),
            )
        };
        let spo = LayeredIndex::from_base(take5(&mut it));
        let pos = LayeredIndex::from_base(take5(&mut it));
        let osp = LayeredIndex::from_base(take5(&mut it));

        let dict = Dictionary::from_mapped(map.clone(), dict_off);

        let mut store = TripleStore::new();
        store.dict = dict;
        store.spo = spo;
        store.pos = pos;
        store.osp = osp;
        store.rebuild_aux();
        Ok(store)
    }

    /// Picks the permutation with the most leading bound variables.
    ///
    /// Thanks to the three permutations SPO, POS, OSP, any query with at least
    /// one bound variable can be rotated so that variable comes first. With
    /// exactly one free variable, it can always be rotated to the last position,
    /// so the result can be returned as a flat slice (no allocation).
    pub fn query(&self, s: Term, p: Term, o: Term) -> QueryResult<'_> {
        match (s, p, o) {
            // -----------------------------------------------------------
            // 0 free variables
            // -----------------------------------------------------------
            (Term::Bound(sv), Term::Bound(pv), Term::Bound(ov)) => {
                QueryResult::Exact(self.spo.contains(sv, pv, ov))
            }

            // -----------------------------------------------------------
            // 1 free variable -> always the last position in a permutation
            // -----------------------------------------------------------
            // (?O) via SPO
            (Term::Bound(sv), Term::Bound(pv), Term::Wildcard) => {
                QueryResult::Single(Var::O, self.spo.query_two(sv, pv))
            }
            // (?S) via POS
            (Term::Wildcard, Term::Bound(pv), Term::Bound(ov)) => {
                QueryResult::Single(Var::S, self.pos.query_two(pv, ov))
            }
            // (?P) via OSP
            (Term::Bound(sv), Term::Wildcard, Term::Bound(ov)) => {
                QueryResult::Single(Var::P, self.osp.query_two(ov, sv))
            }

            // -----------------------------------------------------------
            // 2 free variables -> materialized pairs
            // -----------------------------------------------------------
            // (S, ?P, ?O) via SPO -> pairs (P, O)
            (Term::Bound(sv), Term::Wildcard, Term::Wildcard) => {
                QueryResult::Double(Var::P, Var::O, self.spo.query_one_pairs(sv))
            }
            // (?S, P, ?O) via POS -> permutation (P, O, S), P fixed -> pairs (O, S)
            (Term::Wildcard, Term::Bound(pv), Term::Wildcard) => {
                QueryResult::Double(Var::O, Var::S, self.pos.query_one_pairs(pv))
            }
            // (?S, ?P, O) via OSP -> permutation (O, S, P), O fixed -> pairs (S, P)
            (Term::Wildcard, Term::Wildcard, Term::Bound(ov)) => {
                QueryResult::Double(Var::S, Var::P, self.osp.query_one_pairs(ov))
            }

            // -----------------------------------------------------------
            // 3 free variables -> return everything
            // -----------------------------------------------------------
            (Term::Wildcard, Term::Wildcard, Term::Wildcard) => {
                QueryResult::All(self.spo.all_triples())
            }
        }
    }

    pub fn triple_count(&self) -> usize {
        self.spo.len()
    }

    /// Intersection of the objects of two (S, P, ?O) queries via a fast merge
    /// of the two sorted leaf slices.
    pub fn intersect_objects(&self, s1: u32, p1: u32, s2: u32, p2: u32) -> Vec<u32> {
        let a = self.spo.query_two(s1, p1);
        let b = self.spo.query_two(s2, p2);
        intersect_sorted(&a, &b)
    }

    /// Chain join: (?X, p1, ?Y) AND (?Y, p2, fixed_o).
    ///
    /// Example: (?X, bornIn, ?Y) AND (?Y, locatedIn, Germany).
    ///
    /// Uses POS for both patterns: first all ?Y with (p2, fixed_o), then for
    /// each ?Y all ?X with (p1, ?Y).
    pub fn join_chain(&self, p1: u32, p2: u32, fixed_o: u32) -> Vec<(u32, u32)> {
        let ys = self.pos.query_two(p2, fixed_o);
        let mut result = Vec::new();
        for &y in ys.iter() {
            let xs = self.pos.query_two(p1, y);
            for &x in xs.iter() {
                result.push((x, y));
            }
        }
        result
    }
}

/// Cardinality estimation on demand from the three permutations – replaces the
/// earlier precomputed stats maps (which grew with the triple count and would
/// have occupied tens of GB at WDBench scale). All counts are O(log n) or O(1).
impl CardEstimator for TripleStore {
    fn total(&self) -> usize {
        self.spo.len()
    }
    fn sp(&self, s: u32, p: u32) -> usize {
        self.spo.count_two(s, p)
    }
    fn po(&self, p: u32, o: u32) -> usize {
        self.pos.count_two(p, o)
    }
    fn os(&self, o: u32, s: u32) -> usize {
        self.osp.count_two(o, s)
    }
    fn sdeg(&self, s: u32) -> usize {
        self.spo.count_one(s)
    }
    fn pdeg(&self, p: u32) -> usize {
        self.pos.count_one(p)
    }
    fn odeg(&self, o: u32) -> usize {
        self.osp.count_one(o)
    }
}

impl Default for TripleStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Inserts `val` into `vec` in sorted order, if not already present.
fn sorted_insert(vec: &mut Vec<u32>, val: u32) {
    if let Err(pos) = vec.binary_search(&val) {
        vec.insert(pos, val);
    }
}

/// Removes `val` from the sorted `vec`, if present.
fn sorted_remove(vec: &mut Vec<u32>, val: u32) {
    if let Ok(pos) = vec.binary_search(&val) {
        vec.remove(pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cardinality_estimator_from_index() {
        // CardEstimator is served on-demand from the indexes (no stored maps).
        // s1 -p-> {o1,o2}, s2 -p-> o1, s1 -q-> o1
        let mut store = TripleStore::new();
        let s1 = store.dict.insert("s1");
        let s2 = store.dict.insert("s2");
        let p = store.dict.insert("p");
        let q = store.dict.insert("q");
        let o1 = store.dict.insert("o1");
        let o2 = store.dict.insert("o2");
        store.insert_triple(s1, p, o1);
        store.insert_triple(s1, p, o2);
        store.insert_triple(s2, p, o1);
        store.insert_triple(s1, q, o1);

        assert_eq!(store.total(), 4);
        assert_eq!(store.sp(s1, p), 2); // #objects of (s1,p)
        assert_eq!(store.sp(s2, p), 1);
        assert_eq!(store.po(p, o1), 2); // #subjects of (p,o1): s1,s2
        assert_eq!(store.os(o1, s1), 2); // #predicates of (o1,s1): p,q
        assert_eq!(store.sdeg(s1), 3); // triples with subject s1
        assert_eq!(store.pdeg(p), 3); // triples with predicate p
        assert_eq!(store.odeg(o1), 3); // triples with object o1
        assert_eq!(store.sp(s1, q), 1);
        assert_eq!(store.po(q, o2), 0); // none
    }

    fn example_store() -> TripleStore {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            ("alice", "knows", "bob"),
            ("alice", "knows", "charlie"),
            ("alice", "age", "30"),
            ("bob", "knows", "alice"),
            ("bob", "age", "25"),
            ("charlie", "knows", "bob"),
        ]);
        store
    }

    #[test]
    fn exact_query() {
        let store = example_store();
        assert!(matches!(
            store.query(Term::Bound(0), Term::Bound(1), Term::Bound(2)),
            QueryResult::Exact(true)
        ));
    }

    #[test]
    fn wildcard_object() {
        let store = example_store();
        if let QueryResult::Single(Var::O, vals) =
            store.query(Term::Bound(0), Term::Bound(1), Term::Wildcard)
        {
            assert_eq!(vals.len(), 2);
        } else {
            panic!("expected Single result");
        }
    }

    #[test]
    fn wildcard_subject() {
        let store = example_store();
        if let QueryResult::Single(Var::S, vals) =
            store.query(Term::Wildcard, Term::Bound(1), Term::Bound(2))
        {
            assert_eq!(vals.len(), 2); // alice and charlie know bob
        } else {
            panic!("expected Single result");
        }
    }

    #[test]
    fn wildcard_predicate() {
        let store = example_store();
        if let QueryResult::Single(Var::P, vals) =
            store.query(Term::Bound(0), Term::Wildcard, Term::Bound(2))
        {
            assert_eq!(vals.len(), 1); // alice knows bob
        } else {
            panic!("expected Single result");
        }
    }

    #[test]
    fn all_wildcards() {
        let store = example_store();
        if let QueryResult::All(triples) =
            store.query(Term::Wildcard, Term::Wildcard, Term::Wildcard)
        {
            assert_eq!(triples.len(), 6);
        } else {
            panic!("expected All result");
        }
    }

    #[test]
    fn intersect_objects_finds_common_friends() {
        // Should (alice, knows, ?O) ∩ (bob, knows, ?O) contain [alice]?
        // alice knows bob & charlie; bob knows alice.
        // Intersection: {} (bob and alice share no acquaintances)
        //
        // Better: alice and charlie both know bob.
        // (alice, knows, ?O) ∩ (charlie, knows, ?O) = [bob]
        let store = example_store();
        let alice = store.dict.lookup_iri("alice").unwrap();
        let charlie = store.dict.lookup_iri("charlie").unwrap();
        let bob = store.dict.lookup_iri("bob").unwrap();
        let knows = store.dict.lookup_iri("knows").unwrap();

        let common = store.intersect_objects(alice, knows, charlie, knows);
        assert_eq!(common, vec![bob]);
    }

    #[test]
    fn dump_ntriples_roundtrip_preserves_literals() {
        use super::super::export::parse_ntriples;

        let mut store = TripleStore::new();
        store.ingest_str_triples(&[(
            "http://example.org/alice",
            "http://example.org/knows",
            "http://example.org/bob",
        )]);
        // Insert one literal with a language tag and one with a datatype.
        let s = store.dict.insert_with_type(
            "http://example.org/alice",
            super::super::dictionary::TermType::Iri,
        );
        let name = store.dict.insert_with_type(
            "http://example.org/name",
            super::super::dictionary::TermType::Iri,
        );
        let lit = store.dict.insert_with_type(
            "Alice",
            super::super::dictionary::TermType::literal_lang("en"),
        );
        store.insert_triple(s, name, lit);

        let path = std::env::temp_dir().join("trillian_roundtrip.nt");
        let path_str = path.to_str().unwrap();
        store.dump_ntriples(path_str).unwrap();

        let reparsed = parse_ntriples(path_str).unwrap();
        let mut store2 = TripleStore::new();
        store2.ingest(&reparsed);

        assert_eq!(store.triple_count(), store2.triple_count());
        // The language literal must be preserved losslessly.
        let lit2 = store2
            .dict
            .lookup_term(
                "Alice",
                &super::super::dictionary::TermType::literal_lang("en"),
            )
            .expect("literal preserved");
        assert!(matches!(
            store2.dict.resolve_type(lit2),
            Some(super::super::dictionary::TermType::Literal { lang: Some(_), .. })
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn snapshot_mmap_roundtrip() {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            (
                "http://example.org/a",
                "http://example.org/p",
                "http://example.org/b",
            ),
            (
                "http://example.org/a",
                "http://example.org/p",
                "http://example.org/c",
            ),
            (
                "http://example.org/b",
                "http://example.org/q",
                "http://example.org/c",
            ),
        ]);
        // also include a typed literal
        let s = store.dict.insert("http://example.org/a");
        let name = store.dict.insert("http://example.org/name");
        let lit = store.dict.insert_with_type(
            "Alice",
            super::super::dictionary::TermType::literal_lang("en"),
        );
        store.insert_triple(s, name, lit);

        let path = std::env::temp_dir().join("trillian_snapshot.bin");
        let path_str = path.to_str().unwrap();
        store.save_snapshot(path_str).unwrap();

        let loaded = TripleStore::load_snapshot(path_str).unwrap();
        assert_eq!(loaded.triple_count(), store.triple_count());

        // Query over the mapped index
        let p = loaded.dict.lookup_iri("http://example.org/p").unwrap();
        let a = loaded.dict.lookup_iri("http://example.org/a").unwrap();
        if let QueryResult::Single(Var::O, objs) =
            loaded.query(Term::Bound(a), Term::Bound(p), Term::Wildcard)
        {
            assert_eq!(objs.len(), 2); // b, c
        } else {
            panic!("expected Single");
        }

        // Literal type lossless
        let lit2 = loaded
            .dict
            .lookup_term(
                "Alice",
                &super::super::dictionary::TermType::literal_lang("en"),
            )
            .unwrap();
        assert!(matches!(
            loaded.dict.resolve_type(lit2),
            Some(super::super::dictionary::TermType::Literal { lang: Some(_), .. })
        ));

        // WCOJ helper lists derived correctly
        assert!(loaded.has_predicate(p));

        // mmap base: IRI resolves fully (unfolded).
        assert_eq!(
            loaded.dict.resolve(a).as_deref(),
            Some("http://example.org/a")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn snapshot_insert_after_load_uses_overlay() {
        // Terms added after loading land in the owned overlay (id >= base_n),
        // without duplicating the mmap base; updates stay functional.
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[(
            "http://www.wikidata.org/entity/Q1",
            "http://www.wikidata.org/prop/direct/P1",
            "http://www.wikidata.org/entity/Q2",
        )]);
        let path = std::env::temp_dir().join("trillian_overlay_snapshot.bin");
        let ps = path.to_str().unwrap();
        store.save_snapshot(ps).unwrap();
        let mut loaded = TripleStore::load_snapshot(ps).unwrap();
        let base = loaded.dict.len();

        // existing (mapped) term -> same ID, no new entry
        let q1 = loaded.dict.insert("http://www.wikidata.org/entity/Q1");
        assert!((q1 as usize) < base, "mapped term keeps its base ID");
        assert_eq!(loaded.dict.len(), base, "no duplicate in the overlay");

        // new term -> overlay ID >= base, resolvable + findable correctly
        let q3 = loaded.dict.insert("http://www.wikidata.org/entity/Q3");
        assert!((q3 as usize) >= base, "new term in the overlay");
        assert_eq!(
            loaded.dict.resolve(q3).as_deref(),
            Some("http://www.wikidata.org/entity/Q3")
        );
        assert_eq!(
            loaded.dict.lookup_iri("http://www.wikidata.org/entity/Q3"),
            Some(q3)
        );
        // an insert + query over the new term
        loaded.insert_triple(q1, q1, q3);
        assert!(loaded.spo.contains(q1, q1, q3));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_snapshot_rejects_bad_magic_and_version() {
        // Garbage bytes -> error instead of panic.
        let bad = std::env::temp_dir().join("trillian_bad_snapshot.bin");
        std::fs::write(&bad, vec![0u8; 64]).unwrap();
        assert!(TripleStore::load_snapshot(bad.to_str().unwrap()).is_err());
        let _ = std::fs::remove_file(&bad);

        // Write a valid snapshot, then corrupt the version number.
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[(
            "http://example.org/a",
            "http://example.org/p",
            "http://example.org/b",
        )]);
        let path = std::env::temp_dir().join("trillian_versioned_snapshot.bin");
        let ps = path.to_str().unwrap();
        store.save_snapshot(ps).unwrap();
        assert!(TripleStore::load_snapshot(ps).is_ok());

        let mut bytes = std::fs::read(ps).unwrap();
        bytes[8] = 0xFF; // break the version byte
        std::fs::write(ps, &bytes).unwrap();
        assert!(TripleStore::load_snapshot(ps).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn apply_updates_single_rebuild() {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[(
            "http://example.org/a",
            "http://example.org/p",
            "http://example.org/b",
        )]);
        let a = store.dict.lookup_iri("http://example.org/a").unwrap();
        let p = store.dict.lookup_iri("http://example.org/p").unwrap();
        let b = store.dict.lookup_iri("http://example.org/b").unwrap();
        let c = store.dict.insert("http://example.org/c");

        // delete b, insert c – in one rebuild.
        store.apply_updates(&[(a, p, c)], &[(a, p, b)]);
        assert_eq!(store.triple_count(), 1);
        assert!(matches!(
            store.query(Term::Bound(a), Term::Bound(p), Term::Bound(c)),
            QueryResult::Exact(true)
        ));
        assert!(matches!(
            store.query(Term::Bound(a), Term::Bound(p), Term::Bound(b)),
            QueryResult::Exact(false)
        ));
    }

    #[test]
    fn join_chain_born_in_located_in() {
        // Manual mini dataset:
        // city0 locatedIn country0
        // person0 bornIn city0
        // person1 bornIn city0
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            ("city0", "locatedIn", "country0"),
            ("person0", "bornIn", "city0"),
            ("person1", "bornIn", "city0"),
            ("person2", "bornIn", "city1"), // different city
        ]);

        let born_in = store.dict.lookup_iri("bornIn").unwrap();
        let located_in = store.dict.lookup_iri("locatedIn").unwrap();
        let country0 = store.dict.lookup_iri("country0").unwrap();

        let results = store.join_chain(born_in, located_in, country0);
        assert_eq!(results.len(), 2);
    }
}
