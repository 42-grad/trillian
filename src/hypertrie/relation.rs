use std::collections::BTreeMap;

/// Eine binäre Relation für ein festes Prädikat P: (S, O) Paare.
///
/// Gespeichert als zwei `BTreeMap`-CSRs:
/// * Forward:  S -> sortierte O-Werte
/// * Reverse:  O -> sortierte S-Werte
///
/// Zusätzlich werden die sortierten, duplikatfreien Listen aller Subjekte
/// bzw. Objekte als flache `Vec` gehalten (`all_subjects`/`all_objects`),
/// da der WCOJ-Executor diese als zusammenhängende Slices für ungebundene
/// Variablen benötigt.
///
/// Das Layout ermöglicht Worst-Case-Optimal-Joins **und** inkrementelle
/// `insert`/`delete` in O(log n + Blattgröße) ohne Komplett-Neuaufbau.
#[derive(Debug, Clone, Default)]
pub struct PredicateRelation {
    pub pid: u32,

    forward: BTreeMap<u32, Vec<u32>>,
    reverse: BTreeMap<u32, Vec<u32>>,

    all_subjects: Vec<u32>,
    all_objects: Vec<u32>,
}

impl PredicateRelation {
    pub fn empty(pid: u32) -> Self {
        Self {
            pid,
            ..Default::default()
        }
    }

    /// Baut die Relation effizient aus allen Tripeln mit Prädikat `pid`.
    pub fn build(pid: u32, triples: &[(u32, u32, u32)]) -> Self {
        let mut rel = Self::empty(pid);

        let mut pairs: Vec<(u32, u32)> = triples
            .iter()
            .filter(|(_, p, _)| *p == pid)
            .map(|(s, _, o)| (*s, *o))
            .collect();

        // Forward: sortiert nach (S, O), Blätter in einem Durchlauf füllen.
        pairs.sort_unstable();
        for &(s, o) in &pairs {
            let leaf = rel.forward.entry(s).or_default();
            if leaf.last() != Some(&o) {
                leaf.push(o);
            }
        }

        // Reverse: sortiert nach (O, S).
        pairs.sort_unstable_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        for &(s, o) in &pairs {
            let leaf = rel.reverse.entry(o).or_default();
            if leaf.last() != Some(&s) {
                leaf.push(s);
            }
        }

        // BTreeMap-Keys sind bereits sortiert.
        rel.all_subjects = rel.forward.keys().copied().collect();
        rel.all_objects = rel.reverse.keys().copied().collect();
        rel
    }

    /// Fügt das Paar (s, o) ein (idempotent).
    pub fn insert(&mut self, s: u32, o: u32) {
        let leaf = self.forward.entry(s).or_default();
        match leaf.binary_search(&o) {
            Ok(_) => return, // bereits vorhanden -> nichts zu tun
            Err(pos) => leaf.insert(pos, o),
        }
        if let Err(pos) = self.all_subjects.binary_search(&s) {
            self.all_subjects.insert(pos, s);
        }

        let rleaf = self.reverse.entry(o).or_default();
        if let Err(pos) = rleaf.binary_search(&s) {
            rleaf.insert(pos, s);
        }
        if let Err(pos) = self.all_objects.binary_search(&o) {
            self.all_objects.insert(pos, o);
        }
    }

    /// Entfernt das Paar (s, o), falls vorhanden. Räumt leere Knoten auf.
    pub fn delete(&mut self, s: u32, o: u32) {
        let Some(leaf) = self.forward.get_mut(&s) else {
            return;
        };
        let Ok(pos) = leaf.binary_search(&o) else {
            return;
        };
        leaf.remove(pos);
        if leaf.is_empty() {
            self.forward.remove(&s);
            if let Ok(p) = self.all_subjects.binary_search(&s) {
                self.all_subjects.remove(p);
            }
        }

        if let Some(rleaf) = self.reverse.get_mut(&o) {
            if let Ok(rpos) = rleaf.binary_search(&s) {
                rleaf.remove(rpos);
            }
            if rleaf.is_empty() {
                self.reverse.remove(&o);
                if let Ok(p) = self.all_objects.binary_search(&o) {
                    self.all_objects.remove(p);
                }
            }
        }
    }

    /// `true`, wenn die Relation keine Paare mehr enthält.
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    #[inline]
    pub fn objects_for(&self, subject: u32) -> &[u32] {
        self.forward.get(&subject).map_or(&[], |v| v.as_slice())
    }

    #[inline]
    pub fn subjects_for(&self, object: u32) -> &[u32] {
        self.reverse.get(&object).map_or(&[], |v| v.as_slice())
    }

    #[inline]
    pub fn all_subjects(&self) -> &[u32] {
        &self.all_subjects
    }

    #[inline]
    pub fn all_objects(&self) -> &[u32] {
        &self.all_objects
    }
}
