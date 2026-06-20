use std::borrow::Cow;

use rustc_hash::FxHashMap;

/// Kompakte, **flache CSR-Arena** für eine feste Triple-Permutation
/// (z. B. SPO, POS, OSP) plus kleines **Delta-Overlay** für inkrementelle
/// Updates.
///
/// Motivation: Die frühere `BTreeMap<u32, BTreeMap<u32, Vec<u32>>>`-Variante
/// erzeugte Millionen winziger Heap-Allokationen (eine `Vec` pro Blatt +
/// BTreeMap-Knoten) und dominierte damit den RSS. Die flache CSR-Basis hält
/// alle Daten in wenigen großen, zusammenhängenden Vektoren.
///
/// Damit Updates trotzdem inkrementell und schnell bleiben (flache CSR kann
/// nicht billig in der Mitte einfügen), liegt darüber ein Delta:
/// * `ins`: pro `(first, second)` die hinzugefügten, sortierten `third`-Werte
///   (disjunkt zur Basis).
/// * `del`: pro `(first, second)` die gelöschten, sortierten `third`-Werte.
///
/// Wächst das Delta zu groß, wird es per [`compact`](Self::compact) in eine
/// neue flache Basis gefaltet.
#[derive(Debug, Clone, Default)]
pub struct LayeredIndex {
    base: FlatCsr,
    ins: FxHashMap<(u32, u32), Vec<u32>>,
    del: FxHashMap<(u32, u32), Vec<u32>>,
    len: usize,
}

/// Unveränderliche, dreistufige flache CSR-Struktur.
#[derive(Debug, Clone, Default)]
struct FlatCsr {
    keys: Vec<u32>,        // sortierte, distinkte first-Werte
    key_off: Vec<u32>,     // keys.len()+1; Bereich in l1
    l1: Vec<u32>,          // pro first: sortierte distinkte second-Werte
    l1_off: Vec<u32>,      // l1.len()+1; Bereich in vals
    vals: Vec<u32>,        // pro (first, second): sortierte distinkte thirds
}

impl FlatCsr {
    /// Baut die flache CSR aus einer (unsortierten) Triple-Liste.
    fn build(triples: &[(u32, u32, u32)]) -> Self {
        let mut sorted = triples.to_vec();
        sorted.sort_unstable();
        sorted.dedup();

        let mut csr = FlatCsr {
            key_off: vec![0],
            l1_off: vec![0],
            ..Default::default()
        };

        let mut i = 0;
        while i < sorted.len() {
            let first = sorted[i].0;
            csr.keys.push(first);
            while i < sorted.len() && sorted[i].0 == first {
                let second = sorted[i].1;
                csr.l1.push(second);
                while i < sorted.len() && sorted[i].0 == first && sorted[i].1 == second {
                    csr.vals.push(sorted[i].2);
                    i += 1;
                }
                csr.l1_off.push(csr.vals.len() as u32);
            }
            csr.key_off.push(csr.l1.len() as u32);
        }
        csr
    }

    /// Absoluter l1-Index für `(first, second)`, falls vorhanden.
    #[inline]
    fn leaf_index(&self, first: u32, second: u32) -> Option<usize> {
        let i = self.keys.binary_search(&first).ok()?;
        let start = self.key_off[i] as usize;
        let end = self.key_off[i + 1] as usize;
        self.l1[start..end].binary_search(&second).ok().map(|p| p + start)
    }

    #[inline]
    fn query_two(&self, first: u32, second: u32) -> &[u32] {
        match self.leaf_index(first, second) {
            Some(j) => {
                let s = self.l1_off[j] as usize;
                let e = self.l1_off[j + 1] as usize;
                &self.vals[s..e]
            }
            None => &[],
        }
    }

    #[inline]
    fn contains(&self, first: u32, second: u32, third: u32) -> bool {
        self.query_two(first, second).binary_search(&third).is_ok()
    }

    fn all_triples(&self) -> Vec<(u32, u32, u32)> {
        let mut out = Vec::with_capacity(self.vals.len());
        for (i, &first) in self.keys.iter().enumerate() {
            let ls = self.key_off[i] as usize;
            let le = self.key_off[i + 1] as usize;
            for j in ls..le {
                let second = self.l1[j];
                let vs = self.l1_off[j] as usize;
                let ve = self.l1_off[j + 1] as usize;
                for &third in &self.vals[vs..ve] {
                    out.push((first, second, third));
                }
            }
        }
        out
    }

    #[inline]
    fn len(&self) -> usize {
        self.vals.len()
    }
}

impl LayeredIndex {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Baut den Index (flache Basis, leeres Delta) aus einer Triple-Liste.
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

    /// Anzahl Delta-Einträge (für Kompaktierungs-Heuristik).
    #[inline]
    fn delta_count(&self) -> usize {
        self.ins.values().map(|v| v.len()).sum::<usize>()
            + self.del.values().map(|v| v.len()).sum::<usize>()
    }

    /// Faltet das Delta in eine frische flache Basis, wenn es relativ zur
    /// Basis zu groß geworden ist. Hält den Speicher beschränkt.
    fn maybe_compact(&mut self) {
        let delta = self.delta_count();
        if delta > 1024 && delta * 4 > self.base.len() {
            self.compact();
        }
    }

    /// Faltet das gesamte Delta in eine neue flache Basis.
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

    /// Fügt `(first, second, third)` ein. Liefert `true`, wenn neu.
    pub fn insert(&mut self, first: u32, second: u32, third: u32) -> bool {
        if self.contains(first, second, third) {
            return false;
        }
        // Falls als gelöscht markiert: Löschung zurücknehmen.
        if remove_from_leaf(&mut self.del, first, second, third) {
            // war in Basis, jetzt wieder sichtbar
        } else {
            insert_into_leaf(&mut self.ins, first, second, third);
        }
        self.len += 1;
        self.maybe_compact();
        true
    }

    /// Entfernt `(first, second, third)`. Liefert `true`, wenn vorhanden.
    pub fn delete(&mut self, first: u32, second: u32, third: u32) -> bool {
        if !self.contains(first, second, third) {
            return false;
        }
        // Falls als Delta-Insert vorhanden: dort entfernen, sonst in del eintragen.
        if remove_from_leaf(&mut self.ins, first, second, third) {
            // war ein Delta-Insert
        } else {
            insert_into_leaf(&mut self.del, first, second, third);
        }
        self.len -= 1;
        self.maybe_compact();
        true
    }

    /// Exakte Existenzprüfung.
    pub fn contains(&self, first: u32, second: u32, third: u32) -> bool {
        if leaf_has(&self.del, first, second, third) {
            return false;
        }
        if leaf_has(&self.ins, first, second, third) {
            return true;
        }
        self.base.contains(first, second, third)
    }

    /// Abfrage `(first, second, ?third)` als sortierter Slice.
    ///
    /// Ohne Delta-Treffer an dieser Stelle wird der Basis-Slice **geliehen**
    /// zurückgegeben (keine Allokation); andernfalls die gemergte Menge
    /// `(Basis ∪ ins) \ del` als `Cow::Owned`.
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

    /// Abfrage `(first, ?second, ?third)`: materialisierte `(second, third)`-Paare.
    pub fn query_one_pairs(&self, first: u32) -> Vec<(u32, u32)> {
        // Distinkte second-Werte unter first sammeln (Basis + Delta-Inserts).
        let mut seconds: Vec<u32> = Vec::new();
        if let Ok(i) = self.base.keys.binary_search(&first) {
            let ls = self.base.key_off[i] as usize;
            let le = self.base.key_off[i + 1] as usize;
            seconds.extend_from_slice(&self.base.l1[ls..le]);
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

    /// Alle gespeicherten Triples in Permutations-Reihenfolge.
    pub fn all_triples(&self) -> Vec<(u32, u32, u32)> {
        if self.ins.is_empty() && self.del.is_empty() {
            return self.base.all_triples();
        }
        let mut out = self.base.all_triples();
        // Löschungen entfernen.
        if !self.del.is_empty() {
            out.retain(|&(f, s, t)| !leaf_has(&self.del, f, s, t));
        }
        // Delta-Inserts ergänzen.
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
}

// --- Delta-Leaf-Hilfsfunktionen (sortierte Vecs pro (first, second)) ---

#[inline]
fn leaf_has(map: &FxHashMap<(u32, u32), Vec<u32>>, first: u32, second: u32, third: u32) -> bool {
    map.get(&(first, second))
        .is_some_and(|v| v.binary_search(&third).is_ok())
}

#[inline]
fn insert_into_leaf(map: &mut FxHashMap<(u32, u32), Vec<u32>>, first: u32, second: u32, third: u32) {
    let leaf = map.entry((first, second)).or_default();
    if let Err(pos) = leaf.binary_search(&third) {
        leaf.insert(pos, third);
    }
}

/// Entfernt `third` aus dem Delta-Leaf; gibt `true` zurück, wenn es da war.
#[inline]
fn remove_from_leaf(map: &mut FxHashMap<(u32, u32), Vec<u32>>, first: u32, second: u32, third: u32) -> bool {
    if let Some(leaf) = map.get_mut(&(first, second)) {
        if let Ok(pos) = leaf.binary_search(&third) {
            leaf.remove(pos);
            if leaf.is_empty() {
                map.remove(&(first, second));
            }
            return true;
        }
    }
    false
}

/// `(base ∪ ins) \ del` für drei sortierte, distinkte Slices.
/// `base` und `ins` sind disjunkt; `del ⊆ base ∪ ins`.
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

/// Schnittmenge zweier sortierter u32-Slices via klassischem Merge.
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

/// Schnittmenge zweier sortierter u32-Slices via Roaring-Bitmap.
pub fn intersect_bitmap(a: &[u32], b: &[u32]) -> Vec<u32> {
    use roaring::RoaringBitmap;

    let bitmap_a: RoaringBitmap = a.iter().copied().collect();
    let bitmap_b: RoaringBitmap = b.iter().copied().collect();
    (&bitmap_a & &bitmap_b).iter().collect()
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
    fn delta_insert_delete_roundtrip() {
        let mut idx = LayeredIndex::build(&[(1, 2, 3), (1, 2, 5)]);
        // Insert in bestehende Gruppe -> merge.
        assert!(idx.insert(1, 2, 4));
        assert_eq!(collect(idx.query_two(1, 2)), vec![3, 4, 5]);
        assert!(!idx.insert(1, 2, 4)); // Duplikat
        assert_eq!(idx.len(), 3);

        // Insert in neue Gruppe.
        assert!(idx.insert(9, 9, 9));
        assert_eq!(collect(idx.query_two(9, 9)), vec![9]);

        // Delete Basis-Wert.
        assert!(idx.delete(1, 2, 3));
        assert_eq!(collect(idx.query_two(1, 2)), vec![4, 5]);
        assert!(!idx.contains(1, 2, 3));

        // Delete Delta-Insert.
        assert!(idx.delete(1, 2, 4));
        assert_eq!(collect(idx.query_two(1, 2)), vec![5]);

        // Re-insert eines gelöschten Basis-Werts.
        assert!(idx.insert(1, 2, 3));
        assert_eq!(collect(idx.query_two(1, 2)), vec![3, 5]);

        assert!(!idx.delete(1, 2, 999)); // nicht vorhanden
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
        // Genug Inserts erzwingen, um maybe_compact auszulösen.
        for t in 0..5000u32 {
            idx.insert(2, 2, t);
        }
        // Nach (eventueller) Kompaktierung müssen alle Daten korrekt sein.
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
