use std::collections::BTreeMap;

/// Ein 3-stufiger, **inkrementell aktualisierbarer** Index für eine feste
/// Triple-Permutation, z. B. SPO, POS oder OSP.
///
/// Aufbau: `first -> (second -> sortierte, duplikatfreie thirds)`.
///
/// Im Gegensatz zur früheren flachen CSR-Variante (die bei jeder Änderung
/// komplett neu gebaut werden musste) erlaubt diese `BTreeMap`-Struktur
/// echtes In-Place-`insert`/`delete` in O(log #keys + Blattgröße) – die
/// Grundlage für inkrementelle Updates.
///
/// Wichtig: `query_two` liefert weiterhin einen zusammenhängenden
/// `&[u32]`-Slice (die innere `Vec` eines Blatts), sodass die WCOJ-/Join-
/// Pfade unverändert mit sortierten Slices und `binary_search` arbeiten.
#[derive(Debug, Clone, Default)]
pub struct LayeredIndex {
    map: BTreeMap<u32, BTreeMap<u32, Vec<u32>>>,
    len: usize,
}

impl LayeredIndex {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Baut den Index effizient aus einer (unsortierten) Triple-Liste.
    ///
    /// Sortiert einmal und füllt die Blätter in einem Durchlauf – kein
    /// quadratisches Einfügen pro Element. Duplikate werden entfernt.
    pub fn build(triples: &[(u32, u32, u32)]) -> Self {
        let mut sorted = triples.to_vec();
        sorted.sort_unstable();

        let mut map: BTreeMap<u32, BTreeMap<u32, Vec<u32>>> = BTreeMap::new();
        let mut len = 0usize;
        for &(first, second, third) in &sorted {
            let leaf = map.entry(first).or_default().entry(second).or_default();
            if leaf.last() != Some(&third) {
                leaf.push(third);
                len += 1;
            }
        }

        Self { map, len }
    }

    /// Fügt `(first, second, third)` ein. Liefert `true`, wenn das Triple
    /// neu war (also tatsächlich hinzugefügt wurde).
    pub fn insert(&mut self, first: u32, second: u32, third: u32) -> bool {
        let leaf = self.map.entry(first).or_default().entry(second).or_default();
        match leaf.binary_search(&third) {
            Ok(_) => false,
            Err(pos) => {
                leaf.insert(pos, third);
                self.len += 1;
                true
            }
        }
    }

    /// Entfernt `(first, second, third)`. Liefert `true`, wenn es vorhanden
    /// war. Leere Blätter/Knoten werden aufgeräumt.
    pub fn delete(&mut self, first: u32, second: u32, third: u32) -> bool {
        let Some(inner) = self.map.get_mut(&first) else {
            return false;
        };
        let Some(leaf) = inner.get_mut(&second) else {
            return false;
        };
        let Ok(pos) = leaf.binary_search(&third) else {
            return false;
        };
        leaf.remove(pos);
        self.len -= 1;
        if leaf.is_empty() {
            inner.remove(&second);
        }
        if inner.is_empty() {
            self.map.remove(&first);
        }
        true
    }

    /// Exakte Triple-Existenz: `(first, second, third)`.
    pub fn contains(&self, first: u32, second: u32, third: u32) -> bool {
        self.map
            .get(&first)
            .and_then(|inner| inner.get(&second))
            .is_some_and(|leaf| leaf.binary_search(&third).is_ok())
    }

    /// Abfrage `(first, second, ?third)`.
    /// Liefert einen zusammenhängenden, sortierten Slice der dritten Achse –
    /// ohne Allokation.
    #[inline]
    pub fn query_two(&self, first: u32, second: u32) -> &[u32] {
        self.map
            .get(&first)
            .and_then(|inner| inner.get(&second))
            .map_or(&[], |leaf| leaf.as_slice())
    }

    /// Abfrage `(first, ?second, ?third)`.
    /// Materialisiert `(second, third)`-Paare – nützlich für zwei Wildcards.
    pub fn query_one_pairs(&self, first: u32) -> Vec<(u32, u32)> {
        let mut result = Vec::new();
        if let Some(inner) = self.map.get(&first) {
            for (&second, leaf) in inner {
                for &third in leaf {
                    result.push((second, third));
                }
            }
        }
        result
    }

    /// Alle gespeicherten Triples in der Permutations-Reihenfolge.
    pub fn all_triples(&self) -> Vec<(u32, u32, u32)> {
        let mut result = Vec::with_capacity(self.len);
        for (&first, inner) in &self.map {
            for (&second, leaf) in inner {
                for &third in leaf {
                    result.push((first, second, third));
                }
            }
        }
        result
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

/// Schnittmenge zweier sortierter u32-Slices via klassischem Merge.
/// Cache-freundlich und für kleine Blätter schneller als Bitmap-Aufbau.
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
/// Lohnt sich bei größeren Mengen und wird intern mit SIMD/VEB optimiert.
pub fn intersect_bitmap(a: &[u32], b: &[u32]) -> Vec<u32> {
    use roaring::RoaringBitmap;

    let bitmap_a: RoaringBitmap = a.iter().copied().collect();
    let bitmap_b: RoaringBitmap = b.iter().copied().collect();
    (&bitmap_a & &bitmap_b).iter().collect()
}
