use std::borrow::Cow;
use std::sync::Arc;

use memmap2::Mmap;
use string_interner::backend::StringBackend;
use string_interner::symbol::SymbolU32;
use string_interner::{StringInterner, Symbol};

/// Escape byte for a folded namespace prefix. `0x02` does not occur in IRIs,
/// literals, or language/datatype strings (like `SEP` = `0x01`).
const NS_ESC: char = '\u{2}';

/// [`NS_ESC`] as the byte it appears as in keys.
const NS_ESC_BYTE: u8 = NS_ESC as u8;

/// Known long IRI prefixes. On real Wikidata data `entity/Q*` and
/// `prop/direct/P*` make up the bulk of all IRIs; their prefix (29–37 chars)
/// repeats millions of times. Folding replaces it with 2 bytes (`NS_ESC` +
/// code). **Longest first** (greedy). Index = code offset from 'A'.
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

/// Folds a known prefix of an IRI into `NS_ESC` + a 1-byte code. No match
/// -> borrowed unchanged (e.g. literals, foreign IRIs).
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

/// Inverse of [`fold_iri`].
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

/// String interner: all term strings live in **one** arena (instead of one
/// `String` each), symbols are consecutive 0-based u32 IDs.
type Interner = StringInterner<StringBackend<SymbolU32>>;

/// Type of an RDF term. Stored per dictionary ID so the SPARQL output
/// (term_to_json) can distinguish between IRI, literal with datatype, and
/// literal with language tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

/// Borrowed view of a literal's datatype IRI. Datatype IRIs are
/// namespace-**folded** at ingest (see [`fold_iri`]) — on Wikidata data most
/// are `xsd:*` or Wikidata-prefixed — so the key stores `\x02` + code +
/// suffix, and reconstructing the full IRI would cost one allocation per row.
/// [`DatatypeRef`] keeps the two pieces apart so consumers can compare
/// ([`DatatypeRef::strip_xsd`]) and append ([`DatatypeRef::write_json_body`])
/// without ever unfolding; [`DatatypeRef::unfolded`] allocates only for the
/// callers that genuinely need the whole IRI as one string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DatatypeRef<'a> {
    /// The complete IRI, borrowed when it was stored unfolded.
    Full(Cow<'a, str>),
    /// A folded IRI: the static namespace prefix plus the suffix borrowed
    /// (or owned, for query-local overlay keys) from the key.
    Folded {
        prefix: &'static str,
        rest: Cow<'a, str>,
    },
}

impl DatatypeRef<'_> {
    /// The part after `http://www.w3.org/2001/XMLSchema#`, compared in folded
    /// space — no allocation, no unfolding. `None` for other namespaces.
    pub fn strip_xsd(&self) -> Option<&str> {
        match self {
            DatatypeRef::Full(s) => s.strip_prefix(XSD),
            DatatypeRef::Folded { prefix, rest } if *prefix == XSD => Some(rest),
            _ => None,
        }
    }

    /// Whether the datatype is one of the numeric `xsd:` types.
    pub fn is_numeric_xsd(&self) -> bool {
        matches!(
            self.strip_xsd(),
            Some(
                "integer"
                    | "decimal"
                    | "double"
                    | "float"
                    | "int"
                    | "long"
                    | "short"
                    | "byte"
                    | "nonNegativeInteger"
                    | "positiveInteger"
                    | "nonPositiveInteger"
                    | "negativeInteger"
                    | "unsignedInt"
                    | "unsignedLong"
                    | "unsignedShort"
                    | "unsignedByte"
            )
        )
    }

    /// The full IRI, unfolded only when it was folded (then `Cow::Owned`).
    pub fn unfolded(&self) -> Cow<'_, str> {
        match self {
            DatatypeRef::Full(s) => Cow::Borrowed(s),
            DatatypeRef::Folded { prefix, rest } => {
                let mut s = String::with_capacity(prefix.len() + rest.len());
                s.push_str(prefix);
                s.push_str(rest);
                Cow::Owned(s)
            }
        }
    }

    /// The owned counterpart: suffix and full IRI become `Cow::Owned`.
    pub fn into_owned_datatype(self) -> DatatypeRef<'static> {
        match self {
            DatatypeRef::Full(s) => DatatypeRef::Full(Cow::Owned(s.into_owned())),
            DatatypeRef::Folded { prefix, rest } => DatatypeRef::Folded {
                prefix,
                rest: Cow::Owned(rest.into_owned()),
            },
        }
    }
}

/// Borrowed view of a [`TermType`] for the hot read path. The datatype IRI and
/// language tag borrow straight out of the interned key, so resolving a term's type
/// under every `FILTER`, `ORDER BY`, `BIND` and aggregate allocates **nothing**
/// — `TermType` allocated both strings per row. Convert with
/// [`TermTypeRef::into_owned`] where an owned `TermType` is needed (parsing,
/// WAL, cold paths).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TermTypeRef<'a> {
    Iri,
    Literal {
        datatype: Option<DatatypeRef<'a>>,
        lang: Option<Cow<'a, str>>,
    },
    BlankNode,
}

impl TermTypeRef<'_> {
    /// The owned [`TermType`] counterpart (`Cow::Owned` for the datatype IRI
    /// and language tag).
    pub fn into_owned(self) -> TermType {
        match self {
            TermTypeRef::Iri => TermType::Iri,
            TermTypeRef::BlankNode => TermType::BlankNode,
            TermTypeRef::Literal { datatype, lang } => TermType::Literal {
                datatype: datatype.map(|d| d.unfolded().into_owned()),
                lang: lang.map(|l| l.into_owned()),
            },
        }
    }
}

/// `xsd:string` – a plain literal (`datatype: None`) is, per RDF 1.1,
/// **identical** to a literal explicitly typed with `xsd:string`. Both must
/// therefore get the same dictionary key.
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// The XSD namespace, for folded-datatype comparisons in [`DatatypeRef`].
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// Separator between the type prefix and the lexical value in the interned key.
/// `0x01` does not occur in normal IRIs, datatype IRIs, or language tags; the
/// (arbitrary) lexical value follows as a suffix after the **first** occurrence.
const SEP: char = '\u{1}';

/// Builds the unique interner key from the lexical value **and** type.
///
/// Without the type in the key, `"25"^^xsd:integer`, `"25"^^xsd:string`, and the
/// IRI `25` would collapse to a single ID – a correctness bug for typed-literal
/// constraints. The lexical value stays a contiguous suffix, so [`decode_value`]
/// can slice it zero-copy.
fn encode_key(value: &str, typ: &TermType) -> String {
    match typ {
        // IRI value + datatype IRI are namespace-folded (long Wikidata/XSD
        // prefixes -> 2 bytes). Lexical values of literals stay unchanged.
        TermType::Iri => format!("I{SEP}{}", fold_iri(value)),
        TermType::BlankNode => format!("B{SEP}{value}"),
        TermType::Literal { lang: Some(l), .. } => format!("G{l}{SEP}{value}"),
        TermType::Literal {
            datatype: Some(d),
            lang: None,
        } if d != XSD_STRING => {
            format!("D{}{SEP}{value}", fold_iri(d))
        }
        // plain literal or explicit xsd:string -> same key
        TermType::Literal { .. } => format!("L{SEP}{value}"),
    }
}

/// Recovers the lexical value from an interned key (zero-copy).
#[inline]
fn decode_value(key: &str) -> &str {
    match key.find(SEP) {
        Some(i) => &key[i + SEP.len_utf8()..],
        None => key, // should not happen; defensive
    }
}

/// Reconstructs the term type from the prefix of an interned key.
/// The type is fully encoded in the key (see [`encode_key`]) – a separate
/// `Vec<TermType>` (48 B/term + its own strings) is thus unnecessary.
#[inline]
fn decode_type(key: &str) -> TermType {
    decode_type_ref(key).into_owned()
}

/// [`decode_type`], but borrowing: the datatype IRI and language tag come back
/// as `Cow`s into the key (the datatype namespace-unfolds where folded), so
/// the hot read path never allocates.
fn decode_type_ref(key: &str) -> TermTypeRef<'_> {
    match key.as_bytes().first() {
        Some(b'I') => TermTypeRef::Iri,
        Some(b'B') => TermTypeRef::BlankNode,
        Some(b'L') => TermTypeRef::Literal {
            datatype: None,
            lang: None,
        },
        Some(b'G') => {
            // G<lang>\x01<value>  -> language literal
            let sep = key.find(SEP).unwrap_or(key.len());
            TermTypeRef::Literal {
                datatype: None,
                lang: Some(Cow::Borrowed(&key[1..sep])),
            }
        }
        Some(b'D') => {
            // D<datatype>\x01<value>  -> typed literal. The datatype IRI is
            // namespace-folded in the key; keep prefix and suffix apart
            // instead of unfolding (that would allocate per row).
            let sep = key.find(SEP).unwrap_or(key.len());
            TermTypeRef::Literal {
                datatype: Some(split_datatype(&key[1..sep])),
                lang: None,
            }
        }
        _ => TermTypeRef::Iri, // defensive
    }
}

/// Splits a stored datatype IRI into [`DatatypeRef`] pieces: a namespace-
/// folded IRI (`\x02` + code + suffix) keeps its static prefix, an unfolded
/// one comes back complete. Zero allocation.
fn split_datatype(folded: &str) -> DatatypeRef<'_> {
    let bytes = folded.as_bytes();
    match bytes.first() {
        Some(&NS_ESC_BYTE) if folded.len() >= 2 => {
            let idx = (bytes[1].wrapping_sub(b'A')) as usize;
            match NS_PREFIXES.get(idx) {
                Some(&prefix) => DatatypeRef::Folded {
                    prefix,
                    rest: Cow::Borrowed(&folded[2..]),
                },
                None => DatatypeRef::Full(Cow::Borrowed(folded)),
            }
        }
        _ => DatatypeRef::Full(Cow::Borrowed(folded)),
    }
}

/// [`decode_type_ref`], but the language tag and the datatype suffix are
/// always **owned**. For overlays whose keys do not outlive the call (the
/// query-local term overlay in `src/sparql.rs`); that path is cold.
pub fn decode_type_ref_owned(key: &str) -> TermTypeRef<'static> {
    match key.as_bytes().first() {
        Some(b'G') => {
            let sep = key.find(SEP).unwrap_or(key.len());
            TermTypeRef::Literal {
                datatype: None,
                lang: Some(Cow::Owned(key[1..sep].to_string())),
            }
        }
        Some(b'D') => {
            let sep = key.find(SEP).unwrap_or(key.len());
            TermTypeRef::Literal {
                datatype: Some(split_datatype(&key[1..sep]).into_owned_datatype()),
                lang: None,
            }
        }
        // IRI / blank node / plain literal carry no strings to own.
        _ => match key.as_bytes().first() {
            Some(b'L') => TermTypeRef::Literal {
                datatype: None,
                lang: None,
            },
            _ => TermTypeRef::Iri, // defensive
        },
    }
}

/// Read-only dictionary base, **zero-copy from the mmap snapshot**. Holds no
/// strings in RAM – keys, offsets, and the sorted lookup index live in the
/// mapped file (kept alive via `Arc<Mmap>`). Mirrors the index's base+delta
/// pattern: `MappedDict` = base, `Interner` = overlay.
#[derive(Debug)]
struct MappedDict {
    map: Arc<Mmap>,
    n: usize,
    keys_off: usize,   // byte offset of the key blob
    keys_len: usize,   // length of the blob in bytes
    offs_off: usize,   // byte offset of the u32 offset array (n+1 entries)
    sorted_off: usize, // byte offset of the u32 array of sorted IDs (n entries, lookup)
}

impl MappedDict {
    #[inline]
    fn key_offsets(&self) -> &[u64] {
        // u64: at full scale the blob exceeds 4 GB -> u32 offsets would
        // overflow (365M terms × ~20 B ≈ 7 GB).
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
        // Checked: the snapshot is attacker-controllable, so a corrupt offset or
        // non-UTF-8/boundary-splitting slice must NOT cause UB. Fall back to ""
        // (a corrupt snapshot already failed validation in `from_mapped`; this is
        // defence in depth on the hot path).
        let (lo, hi) = (o[id] as usize, o[id + 1] as usize);
        blob.get(lo..hi)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }
    /// Binary search over the key-sorted IDs. O(log n).
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

/// Bidirectional String ↔ u32 dictionary with term-type information.
///
/// Strings are **interned** (one arena, each key once). The term type is encoded
/// in the key prefix ([`encode_key`]) and reconstructed on demand via
/// [`decode_type`] – no parallel `Vec<TermType>`.
///
/// Two-mode: during build/update all terms live in the `interner` (owned).
/// Loaded from a snapshot, `mapped` forms the **zero-copy** base (IDs
/// `0..base_n`) and the `interner` only takes terms added **after** loading
/// (IDs from `base_n`). This keeps resident RAM low: the term strings live
/// pageable in the mmap instead of owned on the heap.
#[derive(Debug, Default)]
pub struct Dictionary {
    mapped: Option<MappedDict>,
    interner: Interner,
}

impl Clone for Dictionary {
    fn clone(&self) -> Self {
        // Cloned dictionaries share the mmap base (Arc); the overlay is copied.
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

    /// Returns the raw key (incl. type prefix) for an ID – from the mmap base
    /// (id < base_n) or the overlay interner.
    #[inline]
    fn raw_key(&self, id: u32) -> Option<&str> {
        let base = self.base_n();
        if (id as usize) < base {
            Some(self.mapped.as_ref().unwrap().key(id as usize))
        } else {
            SymbolU32::try_from_usize(id as usize - base).and_then(|s| self.interner.resolve(s))
        }
    }

    /// Adds a term with type or returns the existing ID. Terms already present
    /// in the mmap base are **not** duplicated.
    pub fn insert_with_type(&mut self, term: &str, typ: TermType) -> u32 {
        let key = encode_key(term, &typ);
        if let Some(m) = &self.mapped
            && let Some(id) = m.lookup(&key)
        {
            return id;
        }
        self.base_n() as u32 + self.interner.get_or_intern(&key).to_usize() as u32
    }

    /// Adds an IRI term (backward compatibility).
    pub fn insert(&mut self, term: &str) -> u32 {
        self.insert_with_type(term, TermType::Iri)
    }

    /// Returns the ID of a term by lexical value **and** type.
    #[inline]
    pub fn lookup_term(&self, value: &str, typ: &TermType) -> Option<u32> {
        self.lookup_encoded(&encode_key(value, typ))
    }

    /// Same, for a key already built by [`Dictionary::encode`].
    #[inline]
    pub fn lookup_encoded(&self, key: &str) -> Option<u32> {
        if let Some(m) = &self.mapped
            && let Some(id) = m.lookup(key)
        {
            return Some(id);
        }
        self.interner
            .get(key)
            .map(|s| self.base_n() as u32 + s.to_usize() as u32)
    }

    /// The interned-key encoding, for a caller that mints IDs outside the
    /// dictionary and must key terms exactly as it would.
    pub fn encode(value: &str, typ: &TermType) -> String {
        encode_key(value, typ)
    }

    /// Lexical value and type of such a key.
    pub fn decode(key: &str) -> (Cow<'_, str>, TermType) {
        let val = decode_value(key);
        let value = if key.as_bytes().first() == Some(&b'I') {
            unfold_iri(val)
        } else {
            Cow::Borrowed(val)
        };
        (value, decode_type(key))
    }

    /// Convenience: ID of an IRI term.
    #[inline]
    pub fn lookup_iri(&self, iri: &str) -> Option<u32> {
        self.lookup_term(iri, &TermType::Iri)
    }

    /// Resolves an ID to its original lexical value (without the type prefix).
    /// IRIs are namespace-unfolded (then `Cow::Owned`); literals/blank nodes stay
    /// borrowed zero-copy (the most common case on real data).
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

    /// Returns the type of a term (reconstructed from the key prefix).
    #[inline]
    pub fn resolve_type(&self, id: u32) -> Option<TermType> {
        self.raw_key(id).map(|k| decode_type_ref(k).into_owned())
    }

    /// [`Dictionary::resolve_type`], but borrowing: the datatype IRI and
    /// language tag come back as `Cow`s into the mmap'd key, so the hot read
    /// path (`append_term`, `term_to_fv`, …) allocates nothing.
    #[inline]
    pub fn resolve_type_ref(&self, id: u32) -> Option<TermTypeRef<'_>> {
        self.raw_key(id).map(decode_type_ref)
    }

    /// Rough byte estimate of the **owned** RAM (for the memory report). The
    /// mmap base does not count (pageable, zero-copy); only the overlay interner.
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

    /// Serializes the dictionary mmap-friendly. The caller ensures the current
    /// `buf` length is **4-byte aligned**. Layout:
    /// `[n:u32][key_offsets:(n+1)×u64][keys_blob:bytes][pad4][sorted_ids:n×u32]`.
    /// `key_offsets` are cumulative byte offsets into the blob (ID order);
    /// `sorted_ids` are the key-sorted IDs (lookup via binary search).
    pub fn serialize_into(&self, buf: &mut Vec<u8>) {
        let n = self.len();
        buf.extend_from_slice(&(n as u32).to_le_bytes());
        while !buf.len().is_multiple_of(8) {
            buf.push(0); // start the u64 offset array 8-byte aligned
        }
        // key_offsets (cumulative, n+1 entries, u64 because of >4 GB blob at full scale)
        let mut offsets: Vec<u64> = Vec::with_capacity(n + 1);
        let mut acc = 0u64;
        offsets.push(0);
        for id in 0..n as u32 {
            acc += self.raw_key(id).map_or(0, |k| k.len() as u64);
            offsets.push(acc);
        }
        buf.extend_from_slice(bytemuck::cast_slice(&offsets));
        // keys_blob (in ID order, written directly – no intermediate buffer)
        for id in 0..n as u32 {
            buf.extend_from_slice(self.raw_key(id).unwrap_or("").as_bytes());
        }
        while !buf.len().is_multiple_of(4) {
            buf.push(0);
        }
        // sorted_ids: IDs sorted by key
        let mut ids: Vec<u32> = (0..n as u32).collect();
        ids.sort_by(|&a, &b| {
            self.raw_key(a)
                .unwrap_or("")
                .cmp(self.raw_key(b).unwrap_or(""))
        });
        buf.extend_from_slice(bytemuck::cast_slice(&ids));
    }

    /// Builds an **mmap-backed** dictionary from the snapshot (zero-copy, no
    /// owned RAM for the term strings). `dict_off` must be 8-byte aligned.
    ///
    /// Returns `Err` (instead of panicking) if the dictionary section is
    /// truncated or its offsets fall outside the mapped file.
    pub fn from_mapped(map: Arc<Mmap>, dict_off: usize) -> Result<Self, String> {
        let b: &[u8] = &map;
        let rd_u32 = |p: usize| -> Result<u32, String> {
            b.get(p..p + 4)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .ok_or_else(|| format!("dictionary truncated at offset {p}"))
        };
        let rd_u64 = |p: usize| -> Result<u64, String> {
            b.get(p..p + 8)
                .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
                .ok_or_else(|| format!("dictionary truncated at offset {p}"))
        };

        let n = rd_u32(dict_off)? as usize;
        // n:u32 + padding -> u64 offset array starts 8-aligned at dict_off+8.
        let offs_off = dict_off + 8;
        // The offset array has n+1 u64 entries; the last one is the blob length.
        let last_off = offs_off
            .checked_add(n.checked_mul(8).ok_or("dictionary offset overflow")?)
            .ok_or("dictionary offset overflow")?;
        let keys_off = offs_off
            .checked_add((n + 1).checked_mul(8).ok_or("dictionary offset overflow")?)
            .ok_or("dictionary offset overflow")?;
        let keys_len = rd_u64(last_off)? as usize;
        // `keys_end` via checked add — a corrupt `keys_len` must not wrap past
        // `b.len()` and bypass the bounds check.
        let keys_end = keys_off
            .checked_add(keys_len)
            .ok_or("dictionary offset overflow")?;
        let mut sorted_off = keys_end;
        while !sorted_off.is_multiple_of(4) {
            sorted_off += 1;
        }
        // The sorted-id array has n u32 entries; the whole section must fit.
        let end = sorted_off
            .checked_add(n.checked_mul(4).ok_or("dictionary offset overflow")?)
            .ok_or("dictionary offset overflow")?;
        if keys_end > b.len() || end > b.len() {
            return Err("dictionary section out of bounds".to_string());
        }
        // Validate the keys blob is UTF-8 up front, so a corrupt snapshot is
        // rejected at load rather than yielding garbage at query time.
        if std::str::from_utf8(&b[keys_off..keys_end]).is_err() {
            return Err("dictionary keys blob is not valid UTF-8".to_string());
        }
        Ok(Dictionary {
            mapped: Some(MappedDict {
                map: map.clone(),
                n,
                keys_off,
                keys_len,
                offs_off,
                sorted_off,
            }),
            interner: Interner::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same lexical value but different types -> different IDs.
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
        // plain literal and explicit xsd:string are identical per RDF 1.1
        assert_eq!(i_str, i_plain, "plain literal == xsd:string");
    }

    /// `resolve` returns the bare lexical value (without the type prefix), and
    /// `lookup_term` finds exactly the matching typed entry.
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

    /// A lexical value with an embedded separator byte stays intact.
    #[test]
    fn value_containing_separator_byte_roundtrips() {
        let mut d = Dictionary::new();
        let weird = "a\u{1}b";
        let id = d.insert_with_type(weird, TermType::literal_plain());
        assert_eq!(d.resolve(id).as_deref(), Some(weird));
        assert_eq!(d.lookup_term(weird, &TermType::literal_plain()), Some(id));
    }

    /// The borrowed type view agrees with the owned one, and borrows the
    /// datatype/language tag straight out of the key (only a *folded*
    /// datatype IRI comes back as an owned `Cow` after unfolding).
    #[test]
    fn type_ref_matches_owned_and_borrows() {
        let mut d = Dictionary::new();
        let lang = d.insert_with_type("hallo", TermType::literal_lang("de"));
        let xsd = d.insert_with_type(
            "25",
            TermType::literal_datatype("http://www.w3.org/2001/XMLSchema#integer"),
        );
        let custom = d.insert_with_type("x", TermType::literal_datatype("http://example.org/c"));
        let plain = d.insert_with_type("y", TermType::literal_plain());
        let iri = d.insert("http://example.org/i");

        let owned_of_ref = |id: u32| d.resolve_type_ref(id).map(|t| t.into_owned());
        for id in [lang, xsd, custom, plain, iri] {
            assert_eq!(owned_of_ref(id), d.resolve_type(id), "id {id}");
        }
        // Language tag borrows.
        assert!(matches!(
            d.resolve_type_ref(lang),
            Some(TermTypeRef::Literal {
                lang: Some(Cow::Borrowed("de")),
                ..
            })
        ));
        // Unfolded datatype IRI borrows; the folded XSD one keeps pieces apart.
        assert!(matches!(
            d.resolve_type_ref(custom),
            Some(TermTypeRef::Literal {
                datatype: Some(DatatypeRef::Full(Cow::Borrowed("http://example.org/c"))),
                ..
            })
        ));
        assert!(matches!(
            d.resolve_type_ref(xsd),
            Some(TermTypeRef::Literal {
                datatype: Some(DatatypeRef::Folded {
                    prefix: "http://www.w3.org/2001/XMLSchema#",
                    rest: Cow::Borrowed("integer")
                }),
                ..
            })
        ));
        assert!(matches!(
            d.resolve_type_ref(plain),
            Some(TermTypeRef::Literal {
                datatype: None,
                lang: None
            })
        ));
        assert!(matches!(d.resolve_type_ref(iri), Some(TermTypeRef::Iri)));
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
        // Full IRIs come back unchanged (unfolded).
        assert_eq!(d.resolve(iq).as_deref(), Some(q));
        assert_eq!(d.resolve(ip).as_deref(), Some(p));
        assert_eq!(d.resolve(io).as_deref(), Some(other));
        // Lookup via the full IRI finds the folded key.
        assert_eq!(d.lookup_iri(q), Some(iq));
        assert_eq!(d.lookup_iri(p), Some(ip));
        // The folded key is indeed shorter than the full IRI.
        assert!(d.raw_key(iq).unwrap().len() < q.len());
        // Typed literal with an XSD datatype: the datatype is folded + unfolded.
        let dt = "http://www.w3.org/2001/XMLSchema#integer";
        let il = d.insert_with_type("25", TermType::literal_datatype(dt));
        assert_eq!(d.resolve(il).as_deref(), Some("25"));
        match d.resolve_type(il) {
            Some(TermType::Literal {
                datatype: Some(g), ..
            }) => assert_eq!(g, dt),
            other => panic!("expected typed literal, {other:?}"),
        }
    }
}
