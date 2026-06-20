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

    /// Serialisiert das Dictionary in `buf` (für den Snapshot).
    pub fn serialize_into(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&(self.id_to_str.len() as u32).to_le_bytes());
        for (s, t) in self.id_to_str.iter().zip(self.id_to_type.iter()) {
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
            match t {
                TermType::Iri => buf.push(0),
                TermType::BlankNode => buf.push(1),
                TermType::Literal { datatype: None, lang: None } => buf.push(2),
                TermType::Literal { lang: Some(l), .. } => {
                    buf.push(3);
                    buf.extend_from_slice(&(l.len() as u32).to_le_bytes());
                    buf.extend_from_slice(l.as_bytes());
                }
                TermType::Literal { datatype: Some(d), lang: None } => {
                    buf.push(4);
                    buf.extend_from_slice(&(d.len() as u32).to_le_bytes());
                    buf.extend_from_slice(d.as_bytes());
                }
            }
        }
    }

    /// Liest ein Dictionary aus einem (Snapshot-)Byteslice.
    pub fn deserialize(bytes: &[u8]) -> Self {
        let mut dict = Dictionary::new();
        let mut p = 0usize;
        let read_u32 = |b: &[u8], p: &mut usize| -> u32 {
            let v = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
            *p += 4;
            v
        };
        let n = read_u32(bytes, &mut p) as usize;
        dict.id_to_str.reserve(n);
        dict.id_to_type.reserve(n);
        for _ in 0..n {
            let slen = read_u32(bytes, &mut p) as usize;
            let s = std::str::from_utf8(&bytes[p..p + slen]).unwrap().to_string();
            p += slen;
            let tag = bytes[p];
            p += 1;
            let typ = match tag {
                0 => TermType::Iri,
                1 => TermType::BlankNode,
                2 => TermType::literal_plain(),
                3 => {
                    let llen = read_u32(bytes, &mut p) as usize;
                    let l = std::str::from_utf8(&bytes[p..p + llen]).unwrap().to_string();
                    p += llen;
                    TermType::literal_lang(l)
                }
                4 => {
                    let dlen = read_u32(bytes, &mut p) as usize;
                    let d = std::str::from_utf8(&bytes[p..p + dlen]).unwrap().to_string();
                    p += dlen;
                    TermType::literal_datatype(d)
                }
                _ => TermType::Iri,
            };
            let id = dict.id_to_str.len() as u32;
            dict.str_to_id.insert(s.clone(), id);
            dict.id_to_str.push(s);
            dict.id_to_type.push(typ);
        }
        dict
    }
}
