use rustc_hash::{FxHashMap, FxHashSet};

use super::dictionary::{Dictionary, TermType};
use super::export::ParsedTriple;
use super::index::{intersect_sorted, LayeredIndex};
use super::relation::PredicateRelation;
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
    /// Eine freie Variable; der Slice enthält deren Werte.
    Single(Var, &'a [u32]),
    /// Zwei freie Variables; Paare sind materialisiert.
    Double(Var, Var, Vec<(u32, u32)>),
    /// Drei freie Variables; alle Triples materialisiert.
    All(Vec<(u32, u32, u32)>),
}

/// Gesamtspeicher aller drei Permutations-Indizes, Dictionary, Stats und
/// binärer Prädikat-Relationen für WCOJ.
///
/// Die Indizes sind die alleinige Quelle der Wahrheit – es gibt keine
/// separate Triple-Liste mehr. Dadurch sind `insert_triple`/`delete_triple`/
/// `apply_updates` echte **inkrementelle** In-Place-Operationen ohne
/// Komplett-Neuaufbau.
pub struct TripleStore {
    pub dict: Dictionary,
    pub stats: Stats,
    pub relations: FxHashMap<u32, PredicateRelation>,
    spo: LayeredIndex, // Reihenfolge: S, P, O
    pos: LayeredIndex, // Reihenfolge: P, O, S
    osp: LayeredIndex, // Reihenfolge: O, S, P
}

impl TripleStore {
    pub fn new() -> Self {
        Self {
            dict: Dictionary::new(),
            stats: Stats::default(),
            relations: FxHashMap::default(),
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
    /// Relation und die Statistik. Liefert `true`, wenn das Triple neu war.
    fn add_one(&mut self, s: u32, p: u32, o: u32) -> bool {
        let is_new = self.spo.insert(s, p, o);
        if is_new {
            self.pos.insert(p, o, s);
            self.osp.insert(o, s, p);
            self.relations
                .entry(p)
                .or_insert_with(|| PredicateRelation::empty(p))
                .insert(s, o);
            self.stats.add_triple(s, p, o);
        }
        is_new
    }

    /// Inkrementelles Entfernen aus allen Strukturen. Leere Prädikat-
    /// Relationen werden aufgeräumt. Liefert `true`, wenn es vorhanden war.
    fn remove_one(&mut self, s: u32, p: u32, o: u32) -> bool {
        let existed = self.spo.delete(s, p, o);
        if existed {
            self.pos.delete(p, o, s);
            self.osp.delete(o, s, p);
            if let Some(rel) = self.relations.get_mut(&p) {
                rel.delete(s, o);
                if rel.is_empty() {
                    self.relations.remove(&p);
                }
            }
            self.stats.remove_triple(s, p, o);
        }
        existed
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

        // Binäre Relationen pro Prädikat für WCOJ aufbauen.
        let mut predicate_set = FxHashSet::default();
        for (_, p, _) in &triples {
            predicate_set.insert(*p);
        }
        self.relations.clear();
        for p in predicate_set {
            self.relations.insert(p, PredicateRelation::build(p, &triples));
        }

        // Statistik aus den deduplizierten Index-Inhalten ableiten, damit
        // sie konsistent mit den inkrementellen Updates (distinkt) ist.
        self.stats = Stats::default();
        for (s, p, o) in self.spo.all_triples() {
            self.stats.add_triple(s, p, o);
        }
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
        intersect_sorted(a, b)
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
        for &y in ys {
            let xs = self.pos.query_two(p1, y);
            for &x in xs {
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
