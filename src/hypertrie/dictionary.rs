use std::borrow::Cow;
use std::sync::Arc;

use memmap2::Mmap;
use string_interner::backend::StringBackend;
use string_interner::symbol::SymbolU32;
use string_interner::{StringInterner, Symbol};

/// Escape-Byte für ein gefaltetes Namespace-Präfix. `0x02` kommt in IRIs,
/// Literalen und Sprach-/Datentyp-Strings nicht vor (wie `SEP` = `0x01`).
const NS_ESC: char = '\u{2}';

/// Bekannte lange IRI-Präfixe. Bei echten Wikidata-Daten machen `entity/Q*` und
/// `prop/direct/P*` den Großteil aller IRIs aus; ihr Präfix (29–37 Zeichen)
/// wiederholt sich millionenfach. Folding ersetzt ihn durch 2 Bytes
/// (`NS_ESC` + Code). **Längste zuerst** (greedy). Index = Code-Offset ab 'A'.
const NS_PREFIXES: &[&str] = &[
    "http://www.wikidata.org/entity/statement/",
    "http://www.wikidata.org/prop/direct-normalized/",
    "http://www.wikidata.org/prop/direct/",
    "http://www.wikidata.org/prop/statement/",
    "http://www.wikidata.org/prop/qualifier/",
    "http://www.wikidata.org/prop/reference/",
    "http://www.wikidata.org/entity/",
    "http://www.wikidata.org/prop/",
    "http://www.w3.org/2001/XMLSchema#",
    "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
    "http://www.w3.org/2000/01/rdf-schema#",
    "http://www.w3.org/2004/02/skos/core#",
    "http://schema.org/",
];

/// Faltet ein bekanntes Präfix eines IRI in `NS_ESC` + 1-Byte-Code. Kein Treffer
/// -> unverändert geliehen (z. B. Literale, fremde IRIs).
fn fold_iri(iri: &str) -> Cow<'_, str> {
    for (i, pre) in NS_PREFIXES.iter().enumerate() {
        if let Some(rest) = iri.strip_prefix(pre) {
            let mut s = String::with_capacity(2 + rest.len());
            s.push(NS_ESC);
            s.push((b'A' + i as u8) as char);
            s.push_str(rest);
            return Cow::Owned(s);
        }
    }
    Cow::Borrowed(iri)
}

/// Kehrt [`fold_iri`] um.
fn unfold_iri(folded: &str) -> Cow<'_, str> {
    let b = folded.as_bytes();
    if b.first() == Some(&0x02) && b.len() >= 2 {
        let idx = (b[1].wrapping_sub(b'A')) as usize;
        if let Some(pre) = NS_PREFIXES.get(idx) {
            let mut s = String::with_capacity(pre.len() + folded.len());
            s.push_str(pre);
            s.push_str(&folded[2..]);
            return Cow::Owned(s);
        }
    }
    Cow::Borrowed(folded)
}

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
        // IRI-Wert + Datentyp-IRI werden namespace-gefaltet (lange Wikidata/XSD-
        // Präfixe -> 2 Bytes). Lexikalwerte von Literalen bleiben unverändert.
        TermType::Iri => format!("I{SEP}{}", fold_iri(value)),
        TermType::BlankNode => format!("B{SEP}{value}"),
        TermType::Literal { lang: Some(l), .. } => format!("G{l}{SEP}{value}"),
        TermType::Literal {
            datatype: Some(d),
            lang: None,
        } if d != XSD_STRING => {
            format!("D{}{SEP}{value}", fold_iri(d))
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
            // D<datatype>\x01<value>  -> typisiertes Literal (Datentyp entfalten)
            let sep = key.find(SEP).unwrap_or(key.len());
            TermType::literal_datatype(unfold_iri(&key[1..sep]).into_owned())
        }
        _ => TermType::Iri, // defensiv
    }
}

/// Read-only Dictionary-Basis, **zero-copy aus dem mmap-Snapshot**. Hält keine
/// Strings im RAM – Schlüssel, Offsets und der sortierte Lookup-Index liegen in
/// der gemappten Datei (über `Arc<Mmap>` am Leben gehalten). Spiegelt das
/// base+delta-Pattern des Index: `MappedDict` = Basis, `Interner` = Overlay.
#[derive(Debug)]
struct MappedDict {
    map: Arc<Mmap>,
    n: usize,
    keys_off: usize,   // Byte-Offset des Schlüssel-Blobs
    keys_len: usize,   // Länge des Blobs in Bytes
    offs_off: usize,   // Byte-Offset des u32-Offset-Arrays (n+1 Einträge)
    sorted_off: usize, // Byte-Offset des u32-Arrays sortierter IDs (n Einträge, Lookup)
}

impl MappedDict {
    #[inline]
    fn key_offsets(&self) -> &[u64] {
        // u64: der Blob übersteigt bei Vollskala 4 GB -> u32-Offsets würden
        // überlaufen (365M Terme × ~20 B ≈ 7 GB).
        bytemuck::cast_slice(&self.map[self.offs_off..self.offs_off + (self.n + 1) * 8])
    }
    #[inline]
    fn sorted_ids(&self) -> &[u32] {
        bytemuck::cast_slice(&self.map[self.sorted_off..self.sorted_off + self.n * 4])
    }
    #[inline]
    fn key(&self, id: usize) -> &str {
        let o = self.key_offsets();
        let blob = &self.map[self.keys_off..self.keys_off + self.keys_len];
        // SAFETY: serialize schreibt ausschließlich gültige UTF-8-Schlüssel.
        unsafe { std::str::from_utf8_unchecked(&blob[o[id] as usize..o[id + 1] as usize]) }
    }
    /// Binärsuche über die nach Schlüssel sortierten IDs. O(log n).
    fn lookup(&self, key: &str) -> Option<u32> {
        let ids = self.sorted_ids();
        let (mut lo, mut hi) = (0usize, ids.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.key(ids[mid] as usize).cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(ids[mid]),
            }
        }
        None
    }
}

/// Bidirektionales String ↔ u32 Dictionary mit Term-Typ-Information.
///
/// Strings werden **interniert** (eine Arena, jeder Schlüssel einmal). Der
/// Term-Typ ist im Schlüssel-Präfix kodiert ([`encode_key`]) und wird bei Bedarf
/// per [`decode_type`] rekonstruiert – keine parallele `Vec<TermType>`.
///
/// Zweimodig: beim Bau/Update liegen alle Terme im `interner` (owned). Aus einem
/// Snapshot geladen, bildet `mapped` die **zero-copy** Basis (IDs `0..base_n`)
/// und der `interner` nimmt nur **nach** dem Laden hinzugefügte Terme auf
/// (IDs ab `base_n`). Das hält den residenten RAM niedrig: die Term-Strings
/// liegen pageable im mmap statt owned im Heap.
#[derive(Debug, Default)]
pub struct Dictionary {
    mapped: Option<MappedDict>,
    interner: Interner,
}

impl Clone for Dictionary {
    fn clone(&self) -> Self {
        // Geklonte Dictionaries teilen sich die mmap-Basis (Arc), der Overlay
        // wird kopiert.
        Self {
            mapped: self.mapped.as_ref().map(|m| MappedDict {
                map: m.map.clone(),
                n: m.n,
                keys_off: m.keys_off,
                keys_len: m.keys_len,
                offs_off: m.offs_off,
                sorted_off: m.sorted_off,
            }),
            interner: self.interner.clone(),
        }
    }
}

impl Dictionary {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn base_n(&self) -> usize {
        self.mapped.as_ref().map_or(0, |m| m.n)
    }

    /// Liefert den rohen Schlüssel (inkl. Typ-Präfix) zu einer ID – aus der
    /// mmap-Basis (id < base_n) oder dem Overlay-Interner.
    #[inline]
    fn raw_key(&self, id: u32) -> Option<&str> {
        let base = self.base_n();
        if (id as usize) < base {
            Some(self.mapped.as_ref().unwrap().key(id as usize))
        } else {
            SymbolU32::try_from_usize(id as usize - base).and_then(|s| self.interner.resolve(s))
        }
    }

    /// Fügt einen Term mit Typ hinzu oder liefert die existierende ID. Bereits in
    /// der mmap-Basis vorhandene Terme werden **nicht** dupliziert.
    pub fn insert_with_type(&mut self, term: &str, typ: TermType) -> u32 {
        let key = encode_key(term, &typ);
        if let Some(m) = &self.mapped
            && let Some(id) = m.lookup(&key)
        {
            return id;
        }
        self.base_n() as u32 + self.interner.get_or_intern(&key).to_usize() as u32
    }

    /// Fügt einen IRI-Term hinzu (Rückwärtskompatibilität).
    pub fn insert(&mut self, term: &str) -> u32 {
        self.insert_with_type(term, TermType::Iri)
    }

    /// Liefert die ID eines Terms anhand von Lexikalwert **und** Typ.
    #[inline]
    pub fn lookup_term(&self, value: &str, typ: &TermType) -> Option<u32> {
        let key = encode_key(value, typ);
        if let Some(m) = &self.mapped
            && let Some(id) = m.lookup(&key)
        {
            return Some(id);
        }
        self.interner
            .get(&key)
            .map(|s| self.base_n() as u32 + s.to_usize() as u32)
    }

    /// Bequemlichkeit: ID eines IRI-Terms.
    #[inline]
    pub fn lookup_iri(&self, iri: &str) -> Option<u32> {
        self.lookup_term(iri, &TermType::Iri)
    }

    /// Löst eine ID in den ursprünglichen Lexikalwert auf (ohne Typ-Präfix).
    /// IRIs werden namespace-entfaltet (dann `Cow::Owned`); Literale/Blank Nodes
    /// bleiben zero-copy geliehen (der häufigste Fall bei echten Daten).
    #[inline]
    pub fn resolve(&self, id: u32) -> Option<Cow<'_, str>> {
        let key = self.raw_key(id)?;
        let val = decode_value(key);
        if key.as_bytes().first() == Some(&b'I') {
            Some(unfold_iri(val))
        } else {
            Some(Cow::Borrowed(val))
        }
    }

    /// Liefert den Typ eines Terms (aus dem Schlüssel-Präfix rekonstruiert).
    #[inline]
    pub fn resolve_type(&self, id: u32) -> Option<TermType> {
        self.raw_key(id).map(decode_type)
    }

    /// Grobe Byte-Schätzung des **owned** RAM (für den Memory-Report). Die
    /// mmap-Basis zählt nicht (pageable, zero-copy); nur der Overlay-Interner.
    pub fn approx_bytes(&self) -> usize {
        let base = self.base_n() as u32;
        let n_overlay = self.interner.len();
        let str_bytes: usize = (0..n_overlay)
            .filter_map(|i| self.raw_key(base + i as u32))
            .map(|s| s.len())
            .sum();
        str_bytes + n_overlay * 8
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.base_n() + self.interner.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Serialisiert das Dictionary mmap-freundlich. Der Aufrufer stellt sicher,
    /// dass die aktuelle `buf`-Länge **4-Byte-aligned** ist. Layout:
    /// `[n:u32][key_offsets:(n+1)×u32][keys_blob:bytes][pad4][sorted_ids:n×u32]`.
    /// `key_offsets` sind kumulative Byte-Offsets in den Blob (ID-Reihenfolge);
    /// `sorted_ids` sind die nach Schlüssel sortierten IDs (Lookup per Binärsuche).
    pub fn serialize_into(&self, buf: &mut Vec<u8>) {
        let n = self.len();
        buf.extend_from_slice(&(n as u32).to_le_bytes());
        while !buf.len().is_multiple_of(8) {
            buf.push(0); // u64-Offset-Array 8-Byte-aligned beginnen
        }
        // key_offsets (kumulativ, n+1 Einträge, u64 wegen >4 GB Blob bei Vollskala)
        let mut offsets: Vec<u64> = Vec::with_capacity(n + 1);
        let mut acc = 0u64;
        offsets.push(0);
        for id in 0..n as u32 {
            acc += self.raw_key(id).map_or(0, |k| k.len() as u64);
            offsets.push(acc);
        }
        buf.extend_from_slice(bytemuck::cast_slice(&offsets));
        // keys_blob (in ID-Reihenfolge, direkt geschrieben – kein Zwischenpuffer)
        for id in 0..n as u32 {
            buf.extend_from_slice(self.raw_key(id).unwrap_or("").as_bytes());
        }
        while !buf.len().is_multiple_of(4) {
            buf.push(0);
        }
        // sorted_ids: IDs nach Schlüssel sortiert
        let mut ids: Vec<u32> = (0..n as u32).collect();
        ids.sort_by(|&a, &b| {
            self.raw_key(a)
                .unwrap_or("")
                .cmp(self.raw_key(b).unwrap_or(""))
        });
        buf.extend_from_slice(bytemuck::cast_slice(&ids));
    }

    /// Baut ein **mmap-backed** Dictionary aus dem Snapshot (zero-copy, kein
    /// owned RAM für die Term-Strings). `dict_off` muss 8-Byte-aligned sein.
    pub fn from_mapped(map: Arc<Mmap>, dict_off: usize) -> Self {
        let b: &[u8] = &map;
        let n = u32::from_le_bytes(b[dict_off..dict_off + 4].try_into().unwrap()) as usize;
        // n:u32 + Padding -> u64-Offset-Array beginnt 8-aligned bei dict_off+8.
        let offs_off = dict_off + 8;
        let keys_off = offs_off + (n + 1) * 8;
        let keys_len = u64::from_le_bytes(
            b[offs_off + n * 8..offs_off + n * 8 + 8]
                .try_into()
                .unwrap(),
        ) as usize;
        let mut sorted_off = keys_off + keys_len;
        while !sorted_off.is_multiple_of(4) {
            sorted_off += 1;
        }
        Dictionary {
            mapped: Some(MappedDict {
                map: map.clone(),
                n,
                keys_off,
                keys_len,
                offs_off,
                sorted_off,
            }),
            interner: Interner::new(),
        }
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

        assert_eq!(d.resolve(id).as_deref(), Some("25"));
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
        assert_eq!(d.resolve(id).as_deref(), Some(weird));
        assert_eq!(d.lookup_term(weird, &TermType::literal_plain()), Some(id));
    }
}

#[cfg(test)]
mod nsfold {
    use super::*;
    #[test]
    fn namespace_folding_roundtrip() {
        let mut d = Dictionary::new();
        let q = "http://www.wikidata.org/entity/Q42";
        let p = "http://www.wikidata.org/prop/direct/P31";
        let other = "http://example.org/x";
        let iq = d.insert(q);
        let ip = d.insert(p);
        let io = d.insert(other);
        // Volle IRIs kommen unverändert zurück (entfaltet).
        assert_eq!(d.resolve(iq).as_deref(), Some(q));
        assert_eq!(d.resolve(ip).as_deref(), Some(p));
        assert_eq!(d.resolve(io).as_deref(), Some(other));
        // Lookup über die volle IRI findet den gefalteten Schlüssel.
        assert_eq!(d.lookup_iri(q), Some(iq));
        assert_eq!(d.lookup_iri(p), Some(ip));
        // Gefalteter Schlüssel ist tatsächlich kürzer als die volle IRI.
        assert!(d.raw_key(iq).unwrap().len() < q.len());
        // Typisiertes Literal mit XSD-Datentyp: Datentyp wird gefaltet + entfaltet.
        let dt = "http://www.w3.org/2001/XMLSchema#integer";
        let il = d.insert_with_type("25", TermType::literal_datatype(dt));
        assert_eq!(d.resolve(il).as_deref(), Some("25"));
        match d.resolve_type(il) {
            Some(TermType::Literal {
                datatype: Some(g), ..
            }) => assert_eq!(g, dt),
            other => panic!("erwartete typisiertes Literal, {other:?}"),
        }
    }
}
