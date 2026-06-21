use string_interner::backend::StringBackend;
use string_interner::symbol::SymbolU32;
use string_interner::{StringInterner, Symbol};

/// String-Interner: alle Term-Strings liegen in **einer** Arena (statt je einem
/// eigenen `String`), Symbole sind fortlaufende 0-basierte u32-IDs.
type Interner = StringInterner<StringBackend<SymbolU32>>;

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
/// Strings werden **interniert** (eine Arena, jeder String einmal) statt als
/// Millionen einzelner `String`s × 2 gehalten. `id_to_type` ist parallel zu den
/// fortlaufenden Interner-IDs (0-basiert).
#[derive(Debug, Clone, Default)]
pub struct Dictionary {
    interner: Interner,
    id_to_type: Vec<TermType>,
}

impl Dictionary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fügt einen Term mit Typ hinzu oder liefert die existierende ID.
    pub fn insert_with_type(&mut self, term: &str, typ: TermType) -> u32 {
        let id = self.interner.get_or_intern(term).to_usize() as u32;
        // Neuer Term -> ID == bisherige Länge; Typ parallel anhängen.
        if id as usize == self.id_to_type.len() {
            self.id_to_type.push(typ);
        }
        id
    }

    /// Fügt einen IRI-Term hinzu (Rückwärtskompatibilität).
    pub fn insert(&mut self, term: &str) -> u32 {
        self.insert_with_type(term, TermType::Iri)
    }

    /// Liefert die ID eines Terms, falls bekannt.
    #[inline]
    pub fn lookup(&self, term: &str) -> Option<u32> {
        self.interner.get(term).map(|s| s.to_usize() as u32)
    }

    /// Löst eine ID in den ursprünglichen String auf.
    #[inline]
    pub fn resolve(&self, id: u32) -> Option<&str> {
        SymbolU32::try_from_usize(id as usize).and_then(|s| self.interner.resolve(s))
    }

    /// Liefert den Typ eines Terms.
    #[inline]
    pub fn resolve_type(&self, id: u32) -> Option<&TermType> {
        self.id_to_type.get(id as usize)
    }

    /// Grobe Byte-Schätzung (für den Memory-Report). Strings liegen jetzt
    /// **einmal** in der Interner-Arena.
    pub fn approx_bytes(&self) -> usize {
        let n = self.id_to_type.len();
        let str_bytes: usize = (0..n as u32)
            .filter_map(|i| self.resolve(i))
            .map(|s| s.len())
            .sum();
        // Arena-Strings (einmal) + Offsets (~8 B/Eintrag) + Typ-Vec.
        str_bytes + n * 8 + n * std::mem::size_of::<TermType>()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.id_to_type.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.id_to_type.is_empty()
    }

    /// Serialisiert das Dictionary in `buf` (für den Snapshot).
    pub fn serialize_into(&self, buf: &mut Vec<u8>) {
        let n = self.id_to_type.len();
        buf.extend_from_slice(&(n as u32).to_le_bytes());
        for id in 0..n as u32 {
            let s = self.resolve(id).unwrap_or("");
            let t = &self.id_to_type[id as usize];
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
            match t {
                TermType::Iri => buf.push(0),
                TermType::BlankNode => buf.push(1),
                TermType::Literal {
                    datatype: None,
                    lang: None,
                } => buf.push(2),
                TermType::Literal { lang: Some(l), .. } => {
                    buf.push(3);
                    buf.extend_from_slice(&(l.len() as u32).to_le_bytes());
                    buf.extend_from_slice(l.as_bytes());
                }
                TermType::Literal {
                    datatype: Some(d),
                    lang: None,
                } => {
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
        dict.id_to_type.reserve(n);
        for _ in 0..n {
            let slen = read_u32(bytes, &mut p) as usize;
            let s = std::str::from_utf8(&bytes[p..p + slen]).unwrap();
            p += slen;
            let tag = bytes[p];
            p += 1;
            let typ = match tag {
                0 => TermType::Iri,
                1 => TermType::BlankNode,
                2 => TermType::literal_plain(),
                3 => {
                    let llen = read_u32(bytes, &mut p) as usize;
                    let l = std::str::from_utf8(&bytes[p..p + llen])
                        .unwrap()
                        .to_string();
                    p += llen;
                    TermType::literal_lang(l)
                }
                4 => {
                    let dlen = read_u32(bytes, &mut p) as usize;
                    let d = std::str::from_utf8(&bytes[p..p + dlen])
                        .unwrap()
                        .to_string();
                    p += dlen;
                    TermType::literal_datatype(d)
                }
                _ => TermType::Iri,
            };
            dict.insert_with_type(s, typ);
        }
        dict
    }
}
