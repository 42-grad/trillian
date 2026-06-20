use rustc_hash::FxHashMap;

/// Typ eines RDF-Terms. Wird pro Dictionary-ID gespeichert, damit die
/// SPARQL-Ausgabe (term_to_json) zwischen IRI, Literal mit Datentyp und
/// Literal mit Sprach-Tag unterscheiden kann.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermType {
    Iri,
    Literal {
        datatype: Option<String>,
        lang: Option<String>,
    },
    BlankNode,
}

impl TermType {
    pub fn iri() -> Self {
        Self::Iri
    }

    pub fn literal_plain() -> Self {
        Self::Literal {
            datatype: None,
            lang: None,
        }
    }

    pub fn literal_lang(lang: impl Into<String>) -> Self {
        Self::Literal {
            datatype: None,
            lang: Some(lang.into()),
        }
    }

    pub fn literal_datatype(datatype: impl Into<String>) -> Self {
        Self::Literal {
            datatype: Some(datatype.into()),
            lang: None,
        }
    }
}

/// Bidirektionales String ↔ u32 Dictionary mit Term-Typ-Information.
///
/// Design-Entscheidungen:
/// * `FxHashMap` statt `std::collections::HashMap` für deutlich schnellere
///   String-Hashes (weniger Kollisions-Aufwand, bessere Cache-Lokalität).
/// * IDs sind `u32` – ausreichend für Millionen von Termen und halb so groß
///   wie `u64`, was die Index-Arrays dichter packt.
/// * `id_to_str` ist ein flacher `Vec<String>`; der ID-Lookup ist O(1).
/// * Neue Terme bekommen fortlaufende IDs (`id_to_str.len()`).
#[derive(Debug, Clone, Default)]
pub struct Dictionary {
    str_to_id: FxHashMap<String, u32>,
    id_to_str: Vec<String>,
    id_to_type: Vec<TermType>,
}

impl Dictionary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fügt einen Term mit Typ hinzu oder liefert die existierende ID.
    pub fn insert_with_type(&mut self, term: &str, typ: TermType) -> u32 {
        if let Some(&id) = self.str_to_id.get(term) {
            return id;
        }
        let owned = term.to_string();
        let id = self.id_to_str.len() as u32;
        self.str_to_id.insert(owned.clone(), id);
        self.id_to_str.push(owned);
        self.id_to_type.push(typ);
        id
    }

    /// Fügt einen IRI-Term hinzu (Rückwärtskompatibilität).
    pub fn insert(&mut self, term: &str) -> u32 {
        self.insert_with_type(term, TermType::Iri)
    }

    /// Liefert die ID eines Terms, falls bekannt.
    #[inline]
    pub fn lookup(&self, term: &str) -> Option<u32> {
        self.str_to_id.get(term).copied()
    }

    /// Löst eine ID in den ursprünglichen String auf.
    #[inline]
    pub fn resolve(&self, id: u32) -> Option<&str> {
        self.id_to_str.get(id as usize).map(|s| s.as_str())
    }

    /// Liefert den Typ eines Terms.
    #[inline]
    pub fn resolve_type(&self, id: u32) -> Option<&TermType> {
        self.id_to_type.get(id as usize)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.id_to_str.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.id_to_str.is_empty()
    }
}
