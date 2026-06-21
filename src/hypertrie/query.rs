use std::borrow::Cow;

use rustc_hash::FxHashMap;

use super::dictionary::{Dictionary, TermType};
use super::export::ParsedTriple;
use super::index::{intersect_sorted, FlatCsr, LayeredIndex, U32Arena};
use super::stats::Stats;

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
    /// Kein Match.
    Empty,
    /// `(S, P, O)` – exakte Existenzprüfung.
    Exact(bool),
    /// Eine freie Variable; die Werte werden geliehen (kein Delta) oder als
    /// gemergte Menge (mit Delta) zurückgegeben.
    Single(Var, Cow<'a, [u32]>),
    /// Zwei freie Variables; Paare sind materialisiert.
    Double(Var, Var, Vec<(u32, u32)>),
    /// Drei freie Variables; alle Triples materialisiert.
    All(Vec<(u32, u32, u32)>),
}

/// Gesamtspeicher aller drei Permutations-Indizes, Dictionary, Stats und
/// schlanker Prädikat-Schlüssellisten für WCOJ.
///
/// Die Indizes sind die alleinige Quelle der Wahrheit – es gibt keine
/// separate Triple-Liste mehr. Dadurch sind `insert_triple`/`delete_triple`/
/// `apply_updates` echte **inkrementelle** In-Place-Operationen ohne
/// Komplett-Neuaufbau.
///
/// Etappe 1 des Hypertrie-Umbaus: Die früheren Forward/Reverse-CSR-Relationen
/// pro Prädikat (zwei volle Datenkopien) sind entfernt. WCOJ bezieht
/// `objects_for`/`subjects_for` direkt aus den Permutationen (SPO/POS) und
/// braucht zusätzlich nur die sortierten **distinkten** Subjekt-/Objekt-Listen
/// je Prädikat – hier als `pred_subjects`/`pred_objects` gehalten.
pub struct TripleStore {
    pub dict: Dictionary,
    pub stats: Stats,
    /// p -> sortierte, distinkte Subjekte mit Prädikat p (für WCOJ-Kandidaten).
    pred_subjects: FxHashMap<u32, Vec<u32>>,
    /// p -> sortierte, distinkte Objekte mit Prädikat p.
    pred_objects: FxHashMap<u32, Vec<u32>>,
    spo: LayeredIndex, // Reihenfolge: S, P, O
    pos: LayeredIndex, // Reihenfolge: P, O, S
    osp: LayeredIndex, // Reihenfolge: O, S, P
}

impl TripleStore {
    pub fn new() -> Self {
        Self {
            dict: Dictionary::new(),
            stats: Stats::default(),
            pred_subjects: FxHashMap::default(),
            pred_objects: FxHashMap::default(),
            spo: LayeredIndex::empty(),
            pos: LayeredIndex::empty(),
            osp: LayeredIndex::empty(),
        }
    }

    /// Ingest aus geparsten RDF-Tripeln mit Term-Typen.
    pub fn ingest(&mut self, triples: &[ParsedTriple]) {
        let mut id_triples = Vec::with_capacity(triples.len());
        for t in triples {
            let sid = self.dict.insert_with_type(&t.subject.value, t.subject.typ.clone());
            let pid = self.dict.insert_with_type(&t.predicate.value, t.predicate.typ.clone());
            let oid = self.dict.insert_with_type(&t.object.value, t.object.typ.clone());
            id_triples.push((sid, pid, oid));
        }
        self.build_indexes(id_triples);
    }

    /// Wie [`ingest`](Self::ingest), nimmt die geparsten Tripel aber per Wert
    /// und **gibt den String-Puffer frei, bevor die Indizes gebaut werden**.
    pub fn ingest_owned(&mut self, triples: Vec<ParsedTriple>) {
        let mut id_triples = Vec::with_capacity(triples.len());
        for t in &triples {
            let sid = self.dict.insert_with_type(&t.subject.value, t.subject.typ.clone());
            let pid = self.dict.insert_with_type(&t.predicate.value, t.predicate.typ.clone());
            let oid = self.dict.insert_with_type(&t.object.value, t.object.typ.clone());
            id_triples.push((sid, pid, oid));
        }
        drop(triples); // Parse-Puffer (Strings) freigeben
        self.build_indexes(id_triples);
    }

    /// **Streamendes** Laden einer N-Triples-Datei: jede Zeile wird einzeln
    /// geparst und sofort ins Dictionary gemappt (ID-Tripel), ohne jemals den
    /// gesamten `ParsedTriple`-Puffer (~3M Strings) im Speicher zu halten.
    ///
    /// Senkt damit **sowohl** den Peak-RSS (kein Parse-Puffer beim Laden)
    /// **als auch** den resident RSS. Liefert die Anzahl geladener Tripel.
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
                let pid = self.dict.insert_with_type(&t.predicate.value, t.predicate.typ);
                let oid = self.dict.insert_with_type(&t.object.value, t.object.typ);
                id_triples.push((sid, pid, oid));
            }
        }
        let n = id_triples.len();
        self.build_indexes(id_triples);
        Ok(n)
    }

    /// Ingest aus String-Tripeln (Rückwärtskompatibilität; alles als IRI).
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

    /// Direkter Ingest aus bereits gemappten IDs (z. B. für Benchmarks).
    pub fn ingest_id_triples(&mut self, triples: Vec<(u32, u32, u32)>) {
        self.build_indexes(triples);
    }

    /// Fügt ein einzelnes Triple **inkrementell** ein (kein Neuaufbau).
    pub fn insert_triple(&mut self, s: u32, p: u32, o: u32) {
        self.add_one(s, p, o);
    }

    /// Entfernt ein einzelnes Triple **inkrementell** (kein Neuaufbau).
    pub fn delete_triple(&mut self, s: u32, p: u32, o: u32) {
        self.remove_one(s, p, o);
    }

    /// Wendet mehrere Inserts und Deletes inkrementell an. Deletes zuerst.
    /// Jede Operation berührt nur die betroffenen Index-Knoten – es gibt
    /// keinen Komplett-Neuaufbau mehr.
    pub fn apply_updates(&mut self, inserts: &[(u32, u32, u32)], deletes: &[(u32, u32, u32)]) {
        for &(s, p, o) in deletes {
            self.remove_one(s, p, o);
        }
        for &(s, p, o) in inserts {
            self.add_one(s, p, o);
        }
    }

    /// Inkrementelles Einfügen in alle drei Permutationen, die Prädikat-
    /// Schlüssellisten und die Statistik. Liefert `true`, wenn das Triple neu war.
    fn add_one(&mut self, s: u32, p: u32, o: u32) -> bool {
        let is_new = self.spo.insert(s, p, o);
        if is_new {
            self.pos.insert(p, o, s);
            self.osp.insert(o, s, p);
            sorted_insert(self.pred_subjects.entry(p).or_default(), s);
            sorted_insert(self.pred_objects.entry(p).or_default(), o);
            self.stats.add_triple(s, p, o);
        }
        is_new
    }

    /// Inkrementelles Entfernen aus allen Strukturen. Eine Subjekt-/Objekt-ID
    /// fällt nur dann aus der Prädikat-Schlüsselliste, wenn sie unter p kein
    /// weiteres Triple mehr besitzt. Liefert `true`, wenn es vorhanden war.
    fn remove_one(&mut self, s: u32, p: u32, o: u32) -> bool {
        let existed = self.spo.delete(s, p, o);
        if existed {
            self.pos.delete(p, o, s);
            self.osp.delete(o, s, p);

            // s verliert seinen Eintrag in pred_subjects[p] nur, wenn (s,p)
            // kein Objekt mehr hat.
            if self.spo.query_two(s, p).is_empty() {
                if let Some(subs) = self.pred_subjects.get_mut(&p) {
                    sorted_remove(subs, s);
                    if subs.is_empty() {
                        self.pred_subjects.remove(&p);
                    }
                }
            }
            // o verliert seinen Eintrag in pred_objects[p] nur, wenn (p,o)
            // kein Subjekt mehr hat.
            if self.pos.query_two(p, o).is_empty() {
                if let Some(objs) = self.pred_objects.get_mut(&p) {
                    sorted_remove(objs, o);
                    if objs.is_empty() {
                        self.pred_objects.remove(&p);
                    }
                }
            }
            self.stats.remove_triple(s, p, o);
        }
        existed
    }

    // --- WCOJ-/Slice-Zugriffe (ersetzen die früheren PredicateRelation-APIs) ---

    /// Objekte von (s, p) als sortierter Slice – direkt aus dem SPO-Index.
    #[inline]
    pub fn objects_of(&self, s: u32, p: u32) -> Cow<'_, [u32]> {
        self.spo.query_two(s, p)
    }

    /// Subjekte von (p, o) als sortierter Slice – direkt aus dem POS-Index.
    #[inline]
    pub fn subjects_of(&self, p: u32, o: u32) -> Cow<'_, [u32]> {
        self.pos.query_two(p, o)
    }

    /// Sortierte, distinkte Subjekte mit Prädikat p.
    #[inline]
    pub fn subjects_with_predicate(&self, p: u32) -> &[u32] {
        self.pred_subjects.get(&p).map_or(&[], |v| v.as_slice())
    }

    /// Sortierte, distinkte Objekte mit Prädikat p.
    #[inline]
    pub fn objects_with_predicate(&self, p: u32) -> &[u32] {
        self.pred_objects.get(&p).map_or(&[], |v| v.as_slice())
    }

    /// Ob das Prädikat p im Store vorkommt (für WCOJ-Anwendbarkeit).
    #[inline]
    pub fn has_predicate(&self, p: u32) -> bool {
        self.pred_subjects.contains_key(&p)
    }

    /// Schreibt den gesamten Store verlustfrei als N-Triples-Datei.
    ///
    /// Term-Typen (IRI, Literal mit Datentyp/Sprach-Tag) bleiben erhalten,
    /// sodass `parse_ntriples` + `ingest` den Store exakt rekonstruiert.
    /// Dient als einfache, standardkonforme Persistenzschicht.
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
        let value = self.dict.resolve(id).unwrap_or("");
        let default = super::dictionary::TermType::Iri;
        let typ = self.dict.resolve_type(id).unwrap_or(&default);
        super::export::serialize_term(value, typ)
    }

    /// Baut alle Indizes aus einer Triple-Liste neu auf (Bulk-Load).
    ///
    /// Wird nur beim Ingest verwendet; Updates laufen danach inkrementell
    /// über [`add_one`](Self::add_one)/[`remove_one`](Self::remove_one).
    fn build_indexes(&mut self, triples: Vec<(u32, u32, u32)>) {
        // SPO direkt; POS/OSP als Permutationen. `LayeredIndex::build`
        // sortiert und dedupliziert intern.
        self.spo = LayeredIndex::build(&triples);
        let pos: Vec<(u32, u32, u32)> = triples.iter().map(|t| (t.1, t.2, t.0)).collect();
        self.pos = LayeredIndex::build(&pos);
        let osp: Vec<(u32, u32, u32)> = triples.iter().map(|t| (t.2, t.0, t.1)).collect();
        self.osp = LayeredIndex::build(&osp);

        self.rebuild_aux();
    }

    /// Leitet Prädikat-Schlüssellisten und Statistik aus den (bereits
    /// gebauten/gemappten) Permutationen ab. Genutzt nach Bulk-Load **und**
    /// nach dem Laden eines mmap-Snapshots.
    fn rebuild_aux(&mut self) {
        // Prädikat-Schlüssellisten (distinkte Subjekte/Objekte je Prädikat)
        // in O(n) ableiten – die Sortierung erlaubt last()-Deduplikation.
        self.pred_subjects.clear();
        self.pred_objects.clear();
        // SPO ist nach (s,p,o) sortiert -> je Prädikat sind die s monoton.
        for (s, p, _o) in self.spo.all_triples() {
            let subs = self.pred_subjects.entry(p).or_default();
            if subs.last() != Some(&s) {
                subs.push(s);
            }
        }
        // POS ist nach (p,o,s) sortiert -> je Prädikat sind die o monoton.
        for (p, o, _s) in self.pos.all_triples() {
            let objs = self.pred_objects.entry(p).or_default();
            if objs.last() != Some(&o) {
                objs.push(o);
            }
        }

        // Statistik aus den deduplizierten Index-Inhalten ableiten.
        self.stats = Stats::default();
        for (s, p, o) in self.spo.all_triples() {
            self.stats.add_triple(s, p, o);
        }
    }

    /// Druckt eine logische Speicher-Aufschlüsselung (Komponenten in MB).
    /// Logische Schätzung – der reale RSS enthält zusätzlich Allokator-Overhead.
    pub fn memory_report(&self) {
        let mb = |b: usize| b as f64 / 1024.0 / 1024.0;
        let perm = self.spo.heap_bytes() + self.pos.heap_bytes() + self.osp.heap_bytes();
        let dict = self.dict.approx_bytes();
        let pred: usize = self
            .pred_subjects
            .values()
            .map(|v| v.len() * 4)
            .sum::<usize>()
            + self.pred_objects.values().map(|v| v.len() * 4).sum::<usize>();
        let stats = self.stats.approx_bytes();
        let total = perm + dict + pred + stats;
        println!("=== Memory-Report (logisch, {} Triples) ===", self.triple_count());
        println!("  3 Permutationen (SPO/POS/OSP): {:.1} MB", mb(perm));
        println!("  Dictionary (Strings ×2 + Typen): {:.1} MB", mb(dict));
        println!("  Prädikat-Listen:                 {:.1} MB", mb(pred));
        println!(
            "  Stats-Maps ({} Einträge):        {:.1} MB",
            self.stats.entry_count(),
            mb(stats)
        );
        println!("  Summe (logisch):                 {:.1} MB", mb(total));
        println!(
            "  Bytes/Triple (logisch):          {:.0} B",
            total as f64 / self.triple_count().max(1) as f64
        );
    }

    /// Kompaktiert die Deltas aller drei Permutationen in die flachen Basen.
    pub fn compact_all(&mut self) {
        self.spo.compact();
        self.pos.compact();
        self.osp.compact();
    }

    /// Schreibt einen Binär-Snapshot (Dictionary + die 3 flachen CSR-Indizes).
    ///
    /// Die Index-Arrays liegen 4-Byte-aligned hintereinander, sodass sie beim
    /// Laden zero-copy memory-gemappt werden können (vgl. Tentris/metall).
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
        buf.extend_from_slice(b"TTRSNAP1");
        buf.extend_from_slice(&1u32.to_le_bytes()); // version
        buf.extend_from_slice(&(self.dict.len() as u32).to_le_bytes());
        let arrays_off_pos = buf.len();
        buf.extend_from_slice(&0u64.to_le_bytes()); // arrays_offset (Platzhalter)
        let dict_off_pos = buf.len();
        buf.extend_from_slice(&0u64.to_le_bytes()); // dict_offset (Platzhalter)
        for a in &arrays {
            buf.extend_from_slice(&(a.len() as u32).to_le_bytes());
        }
        while buf.len() % 8 != 0 {
            buf.push(0);
        }
        let arrays_off = buf.len() as u64;
        for a in &arrays {
            buf.extend_from_slice(bytemuck::cast_slice::<u32, u8>(a));
        }
        let dict_off = buf.len() as u64;
        self.dict.serialize_into(&mut buf);

        buf[arrays_off_pos..arrays_off_pos + 8].copy_from_slice(&arrays_off.to_le_bytes());
        buf[dict_off_pos..dict_off_pos + 8].copy_from_slice(&dict_off.to_le_bytes());

        std::fs::write(path, buf)
    }

    /// Lädt einen Snapshot per `mmap`: die Index-Arrays werden **zero-copy** in
    /// die Datei gemappt (resident wie bei Tentris), das Dictionary in den RAM
    /// gelesen; Statistik und Prädikatlisten werden abgeleitet.
    pub fn load_snapshot(path: &str) -> std::io::Result<TripleStore> {
        let file = std::fs::File::open(path)?;
        // SAFETY: read-only Snapshot; die Datei wird nicht von außen verändert.
        let map = std::sync::Arc::new(unsafe { memmap2::Mmap::map(&file)? });
        let b: &[u8] = &map;

        let rd_u32 = |b: &[u8], p: usize| u32::from_le_bytes(b[p..p + 4].try_into().unwrap());
        let rd_u64 = |b: &[u8], p: usize| u64::from_le_bytes(b[p..p + 8].try_into().unwrap());

        assert_eq!(&b[0..8], b"TTRSNAP1", "ungültiger Snapshot");
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

        let dict = Dictionary::deserialize(&b[dict_off..]);

        let mut store = TripleStore::new();
        store.dict = dict;
        store.spo = spo;
        store.pos = pos;
        store.osp = osp;
        store.rebuild_aux();
        Ok(store)
    }

    /// Wählt die Permutation mit den meisten führenden gebundenen Variablen.
    ///
    /// Dank der drei Permutationen SPO, POS, OSP lässt sich jede Anfrage mit
    /// mindestens einer gebundenen Variable so drehen, dass diese Variable
    /// an erster Stelle steht. Bei genau einer freien Variable kann diese
    /// immer an die letzte Stelle gedreht werden, sodass das Ergebnis als
    /// flacher Slice zurückgegeben werden kann (keine Allokation).
    pub fn query(&self, s: Term, p: Term, o: Term) -> QueryResult<'_> {
        match (s, p, o) {
            // -----------------------------------------------------------
            // 0 freie Variablen
            // -----------------------------------------------------------
            (Term::Bound(sv), Term::Bound(pv), Term::Bound(ov)) => {
                QueryResult::Exact(self.spo.contains(sv, pv, ov))
            }

            // -----------------------------------------------------------
            // 1 freie Variable -> immer letzte Position in einer Permutation
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
            // 2 freie Variablen -> materialisierte Paare
            // -----------------------------------------------------------
            // (S, ?P, ?O) via SPO -> Paare (P, O)
            (Term::Bound(sv), Term::Wildcard, Term::Wildcard) => {
                QueryResult::Double(Var::P, Var::O, self.spo.query_one_pairs(sv))
            }
            // (?S, P, ?O) via POS -> Permutation (P, O, S), P fest -> Paare (O, S)
            (Term::Wildcard, Term::Bound(pv), Term::Wildcard) => {
                QueryResult::Double(Var::O, Var::S, self.pos.query_one_pairs(pv))
            }
            // (?S, ?P, O) via OSP -> Permutation (O, S, P), O fest -> Paare (S, P)
            (Term::Wildcard, Term::Wildcard, Term::Bound(ov)) => {
                QueryResult::Double(Var::S, Var::P, self.osp.query_one_pairs(ov))
            }

            // -----------------------------------------------------------
            // 3 freie Variablen -> alles zurückgeben
            // -----------------------------------------------------------
            (Term::Wildcard, Term::Wildcard, Term::Wildcard) => {
                QueryResult::All(self.spo.all_triples())
            }
        }
    }

    pub fn triple_count(&self) -> usize {
        self.spo.len()
    }

    /// Schnittmenge der Objekte zweier (S, P, ?O)-Anfragen über einen
    /// schnellen Merge der beiden sortierten Blatt-Slices.
    pub fn intersect_objects(&self, s1: u32, p1: u32, s2: u32, p2: u32) -> Vec<u32> {
        let a = self.spo.query_two(s1, p1);
        let b = self.spo.query_two(s2, p2);
        intersect_sorted(&a, &b)
    }

    /// Chain-Join: (?X, p1, ?Y) AND (?Y, p2, fixed_o).
    ///
    /// Beispiel: (?X, bornIn, ?Y) AND (?Y, locatedIn, Germany).
    ///
    /// Nutzt POS für beide Muster: zuerst alle ?Y mit (p2, fixed_o),
    /// dann für jedes ?Y alle ?X mit (p1, ?Y).
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

impl Default for TripleStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Fügt `val` sortiert in `vec` ein, falls noch nicht vorhanden.
fn sorted_insert(vec: &mut Vec<u32>, val: u32) {
    if let Err(pos) = vec.binary_search(&val) {
        vec.insert(pos, val);
    }
}

/// Entfernt `val` aus dem sortierten `vec`, falls vorhanden.
fn sorted_remove(vec: &mut Vec<u32>, val: u32) {
    if let Ok(pos) = vec.binary_search(&val) {
        vec.remove(pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(vals.len(), 2); // alice und charlie kennen bob
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
        // (alice, knows, ?O) ∩ (bob, knows, ?O) soll [alice] enthalten?
        // alice kennt bob & charlie; bob kennt alice.
        // Schnittmenge: {} (bob und alice haben keine gemeinsamen Bekannten)
        //
        // Besser: alice und charlie kennen beide bob.
        // (alice, knows, ?O) ∩ (charlie, knows, ?O) = [bob]
        let store = example_store();
        let alice = store.dict.lookup("alice").unwrap();
        let charlie = store.dict.lookup("charlie").unwrap();
        let bob = store.dict.lookup("bob").unwrap();
        let knows = store.dict.lookup("knows").unwrap();

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
        // Ein Literal mit Sprach-Tag und eines mit Datentyp einfügen.
        let s = store.dict.insert_with_type("http://example.org/alice", super::super::dictionary::TermType::Iri);
        let name = store.dict.insert_with_type("http://example.org/name", super::super::dictionary::TermType::Iri);
        let lit = store
            .dict
            .insert_with_type("Alice", super::super::dictionary::TermType::literal_lang("en"));
        store.insert_triple(s, name, lit);

        let path = std::env::temp_dir().join("tentris_clone_roundtrip.nt");
        let path_str = path.to_str().unwrap();
        store.dump_ntriples(path_str).unwrap();

        let reparsed = parse_ntriples(path_str).unwrap();
        let mut store2 = TripleStore::new();
        store2.ingest(&reparsed);

        assert_eq!(store.triple_count(), store2.triple_count());
        // Das Sprach-Literal muss verlustfrei erhalten sein.
        let lit2 = store2.dict.lookup("Alice").expect("literal preserved");
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
            ("http://example.org/a", "http://example.org/p", "http://example.org/b"),
            ("http://example.org/a", "http://example.org/p", "http://example.org/c"),
            ("http://example.org/b", "http://example.org/q", "http://example.org/c"),
        ]);
        // ein typisiertes Literal mit aufnehmen
        let s = store.dict.insert("http://example.org/a");
        let name = store.dict.insert("http://example.org/name");
        let lit = store
            .dict
            .insert_with_type("Alice", super::super::dictionary::TermType::literal_lang("en"));
        store.insert_triple(s, name, lit);

        let path = std::env::temp_dir().join("tentris_clone_snapshot.bin");
        let path_str = path.to_str().unwrap();
        store.save_snapshot(path_str).unwrap();

        let loaded = TripleStore::load_snapshot(path_str).unwrap();
        assert_eq!(loaded.triple_count(), store.triple_count());

        // Query über den gemappten Index
        let p = loaded.dict.lookup("http://example.org/p").unwrap();
        let a = loaded.dict.lookup("http://example.org/a").unwrap();
        if let QueryResult::Single(Var::O, objs) = loaded.query(Term::Bound(a), Term::Bound(p), Term::Wildcard)
        {
            assert_eq!(objs.len(), 2); // b, c
        } else {
            panic!("expected Single");
        }

        // Literal-Typ verlustfrei
        let lit2 = loaded.dict.lookup("Alice").unwrap();
        assert!(matches!(
            loaded.dict.resolve_type(lit2),
            Some(super::super::dictionary::TermType::Literal { lang: Some(_), .. })
        ));

        // WCOJ-Hilfslisten korrekt abgeleitet
        assert!(loaded.has_predicate(p));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn apply_updates_single_rebuild() {
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[(
            "http://example.org/a",
            "http://example.org/p",
            "http://example.org/b",
        )]);
        let a = store.dict.lookup("http://example.org/a").unwrap();
        let p = store.dict.lookup("http://example.org/p").unwrap();
        let b = store.dict.lookup("http://example.org/b").unwrap();
        let c = store.dict.insert("http://example.org/c");

        // b löschen, c einfügen – in einem Rebuild.
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
        // Manueller Mini-Datensatz:
        // city0 locatedIn country0
        // person0 bornIn city0
        // person1 bornIn city0
        let mut store = TripleStore::new();
        store.ingest_str_triples(&[
            ("city0", "locatedIn", "country0"),
            ("person0", "bornIn", "city0"),
            ("person1", "bornIn", "city0"),
            ("person2", "bornIn", "city1"), // andere Stadt
        ]);

        let born_in = store.dict.lookup("bornIn").unwrap();
        let located_in = store.dict.lookup("locatedIn").unwrap();
        let country0 = store.dict.lookup("country0").unwrap();

        let results = store.join_chain(born_in, located_in, country0);
        assert_eq!(results.len(), 2);
    }
}
