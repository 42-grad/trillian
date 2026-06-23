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

/// `xsd:string` – ein einfaches Literal (`datatype: None`) ist nach RDF 1.1
/// **identisch** zu einem explizit mit `xsd:string` typisierten Literal. Beide
/// müssen daher denselben Dictionary-Schlüssel erhalten.
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// Trenner zwischen Typ-Präfix und Lexikalwert im internierten Schlüssel.
/// `0x01` kommt in normalen IRIs, Datentyp-IRIs und Sprach-Tags nicht vor; der
/// (beliebige) Lexikalwert steht als Suffix nach dem **ersten** Vorkommen.
const SEP: char = '\u{1}';

/// Baut den eindeutigen Interner-Schlüssel aus Lexikalwert **und** Typ.
///
/// Ohne Typ im Schlüssel kollabieren z. B. `"25"^^xsd:integer`,
/// `"25"^^xsd:string` und der IRI `25` zu einer einzigen ID – ein
/// Korrektheitsfehler bei typisierten Literal-Constraints. Der Lexikalwert
/// bleibt zusammenhängendes Suffix, sodass [`decode_value`] zero-copy slicen kann.
fn encode_key(value: &str, typ: &TermType) -> String {
    match typ {
        TermType::Iri => format!("I{SEP}{value}"),
        TermType::BlankNode => format!("B{SEP}{value}"),
        TermType::Literal { lang: Some(l), .. } => format!("G{l}{SEP}{value}"),
        TermType::Literal {
            datatype: Some(d),
            lang: None,
        } if d != XSD_STRING => {
            format!("D{d}{SEP}{value}")
        }
        // einfaches Literal oder explizit xsd:string -> derselbe Schlüssel
        TermType::Literal { .. } => format!("L{SEP}{value}"),
    }
}

/// Holt den Lexikalwert aus einem internierten Schlüssel zurück (zero-copy).
#[inline]
fn decode_value(key: &str) -> &str {
    match key.find(SEP) {
        Some(i) => &key[i + SEP.len_utf8()..],
        None => key, // sollte nicht vorkommen; defensiv
    }
}

/// Rekonstruiert den Term-Typ aus dem Präfix eines internierten Schlüssels.
/// Der Typ ist vollständig im Schlüssel kodiert (siehe [`encode_key`]) – eine
/// separate `Vec<TermType>` (48 B/Term + eigene Strings) ist damit überflüssig.
fn decode_type(key: &str) -> TermType {
    let bytes = key.as_bytes();
    match bytes.first() {
        Some(b'I') => TermType::Iri,
        Some(b'B') => TermType::BlankNode,
        Some(b'L') => TermType::literal_plain(),
        Some(b'G') => {
            // G<lang>\x01<value>  -> Sprach-Literal
            let sep = key.find(SEP).unwrap_or(key.len());
            TermType::literal_lang(&key[1..sep])
        }
        Some(b'D') => {
            // D<datatype>\x01<value>  -> typisiertes Literal
            let sep = key.find(SEP).unwrap_or(key.len());
            TermType::literal_datatype(&key[1..sep])
        }
        _ => TermType::Iri, // defensiv
    }
}

/// Bidirektionales String ↔ u32 Dictionary mit Term-Typ-Information.
///
/// Strings werden **interniert** (eine Arena, jeder Schlüssel einmal). Der
/// Term-Typ ist im Schlüssel-Präfix kodiert ([`encode_key`]) und wird bei Bedarf
/// per [`decode_type`] rekonstruiert – es gibt **keine** parallele `Vec<TermType>`
/// mehr (sparte bei echten Wikidata-Daten ~270 MB / 10M Tripel: 48 B/Term).
#[derive(Debug, Clone, Default)]
pub struct Dictionary {
    interner: Interner,
}

impl Dictionary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Liefert den rohen internierten Schlüssel (inkl. Typ-Präfix) zu einer ID.
    #[inline]
    fn raw_key(&self, id: u32) -> Option<&str> {
        SymbolU32::try_from_usize(id as usize).and_then(|s| self.interner.resolve(s))
    }

    /// Fügt einen Term mit Typ hinzu oder liefert die existierende ID.
    pub fn insert_with_type(&mut self, term: &str, typ: TermType) -> u32 {
        let key = encode_key(term, &typ);
        self.interner.get_or_intern(&key).to_usize() as u32
    }

    /// Fügt einen IRI-Term hinzu (Rückwärtskompatibilität).
    pub fn insert(&mut self, term: &str) -> u32 {
        self.insert_with_type(term, TermType::Iri)
    }

    /// Liefert die ID eines Terms anhand von Lexikalwert **und** Typ.
    #[inline]
    pub fn lookup_term(&self, value: &str, typ: &TermType) -> Option<u32> {
        self.interner
            .get(encode_key(value, typ))
            .map(|s| s.to_usize() as u32)
    }

    /// Bequemlichkeit: ID eines IRI-Terms.
    #[inline]
    pub fn lookup_iri(&self, iri: &str) -> Option<u32> {
        self.lookup_term(iri, &TermType::Iri)
    }

    /// Löst eine ID in den ursprünglichen Lexikalwert auf (ohne Typ-Präfix).
    #[inline]
    pub fn resolve(&self, id: u32) -> Option<&str> {
        self.raw_key(id).map(decode_value)
    }

    /// Liefert den Typ eines Terms (aus dem Schlüssel-Präfix rekonstruiert).
    #[inline]
    pub fn resolve_type(&self, id: u32) -> Option<TermType> {
        self.raw_key(id).map(decode_type)
    }

    /// Grobe Byte-Schätzung (für den Memory-Report). Nur noch Arena-Strings
    /// (Schlüssel inkl. Präfix, einmal) + Symbol-Offsets – keine Typ-Vec mehr.
    pub fn approx_bytes(&self) -> usize {
        let n = self.len();
        let str_bytes: usize = (0..n as u32)
            .filter_map(|i| self.raw_key(i))
            .map(|s| s.len())
            .sum();
        str_bytes + n * 8
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.interner.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.interner.is_empty()
    }

    /// Serialisiert das Dictionary in `buf` (für den Snapshot): die rohen
    /// internierten Schlüssel (Typ ist im Präfix), in ID-Reihenfolge.
    pub fn serialize_into(&self, buf: &mut Vec<u8>) {
        let n = self.len();
        buf.extend_from_slice(&(n as u32).to_le_bytes());
        for id in 0..n as u32 {
            let key = self.raw_key(id).unwrap_or("");
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(key.as_bytes());
        }
    }

    /// Liest ein Dictionary aus einem (Snapshot-)Byteslice: rohe Schlüssel
    /// direkt re-internieren (ID-Reihenfolge bleibt erhalten).
    pub fn deserialize(bytes: &[u8]) -> Self {
        let mut dict = Dictionary::new();
        let mut p = 0usize;
        let read_u32 = |b: &[u8], p: &mut usize| -> u32 {
            let v = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
            *p += 4;
            v
        };
        let n = read_u32(bytes, &mut p) as usize;
        for _ in 0..n {
            let klen = read_u32(bytes, &mut p) as usize;
            let key = std::str::from_utf8(&bytes[p..p + klen]).unwrap();
            p += klen;
            dict.interner.get_or_intern(key);
        }
        dict
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gleicher Lexikalwert, aber verschiedene Typen -> verschiedene IDs.
    #[test]
    fn distinct_terms_for_same_lexical_value() {
        let mut d = Dictionary::new();
        let i_int = d.insert_with_type(
            "25",
            TermType::literal_datatype("http://www.w3.org/2001/XMLSchema#integer"),
        );
        let i_str = d.insert_with_type("25", TermType::literal_datatype(XSD_STRING));
        let i_iri = d.insert_with_type("25", TermType::Iri);
        let i_plain = d.insert_with_type("25", TermType::literal_plain());

        assert_ne!(i_int, i_str, "integer != string");
        assert_ne!(i_int, i_iri, "integer != IRI");
        assert_ne!(i_str, i_iri, "string != IRI");
        // einfaches Literal und explizites xsd:string sind nach RDF 1.1 identisch
        assert_eq!(i_str, i_plain, "plain literal == xsd:string");
    }

    /// `resolve` liefert den reinen Lexikalwert zurück (ohne Typ-Präfix), und
    /// `lookup_term` findet exakt den passend typisierten Eintrag.
    #[test]
    fn resolve_strips_prefix_and_lookup_is_typed() {
        let mut d = Dictionary::new();
        let dt = "http://www.w3.org/2001/XMLSchema#integer";
        let id = d.insert_with_type("25", TermType::literal_datatype(dt));
        d.insert_with_type("25", TermType::Iri);

        assert_eq!(d.resolve(id), Some("25"));
        assert_eq!(
            d.lookup_term("25", &TermType::literal_datatype(dt)),
            Some(id)
        );
        assert_ne!(d.lookup_iri("25"), Some(id));
        assert_eq!(
            d.lookup_term(
                "25",
                &TermType::literal_datatype("http://www.w3.org/2001/XMLSchema#double")
            ),
            None
        );
    }

    /// Lexikalwert mit eingebettetem Trenner-Byte bleibt unversehrt.
    #[test]
    fn value_containing_separator_byte_roundtrips() {
        let mut d = Dictionary::new();
        let weird = "a\u{1}b";
        let id = d.insert_with_type(weird, TermType::literal_plain());
        assert_eq!(d.resolve(id), Some(weird));
        assert_eq!(d.lookup_term(weird, &TermType::literal_plain()), Some(id));
    }
}
