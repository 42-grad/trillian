use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};

use super::dictionary::TermType;

/// Exportiert String-Tripels im N-Triples-Format (.nt).
///
/// Jede Zeile hat die Form:
/// `<http://example.org/subject> <http://example.org/predicate> <http://example.org/object> .`
///
/// Alle Terme werden als URIs behandelt; Literale werden nicht separat
/// gequotet (für den Tentris-Vergleich ausreichend).
pub fn export_ntriples(path: &str, triples: &[(&str, &str, &str)]) -> std::io::Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);

    for (s, p, o) in triples {
        writeln!(
            writer,
            "<http://example.org/{}> <http://example.org/{}> <http://example.org/{}> .",
            s, p, o
        )?;
    }

    writer.flush()
}

pub struct ParsedTerm {
    pub value: String,
    pub typ: TermType,
}

pub struct ParsedTriple {
    pub subject: ParsedTerm,
    pub predicate: ParsedTerm,
    pub object: ParsedTerm,
}

/// Parst eine N-Triples-Datei (.nt) und liefert (subject, predicate, object)
/// als vollständige Term-Strings inklusive Term-Typ.
///
/// Unterstützt:
///   - IRIs: `<http://example.org/s>`
///   - Literale: `"hello"`, `"hello"@en`, `"30"^^<http://www.w3.org/2001/XMLSchema#integer>`
///   - Escape-Sequenzen in Strings: `\"`, `\\`, `\n`, `\r`, `\t`, `\uXXXX`, `\UXXXXXXXX`
///
/// Blank Nodes (`_:b0`) und syntaktisch ungültige Zeilen werden übersprungen.
pub fn parse_ntriples(path: &str) -> std::io::Result<Vec<ParsedTriple>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut triples = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(triple) = parse_triple_line(line) {
            triples.push(triple);
        }
    }

    Ok(triples)
}

pub fn parse_triple_line(line: &str) -> Option<ParsedTriple> {
    let (s, rest) = parse_term(line)?;
    let rest = skip_whitespace(rest);
    let (p, rest) = parse_term(rest)?;
    let rest = skip_whitespace(rest);
    let (o, rest) = parse_term(rest)?;
    let rest = skip_whitespace(rest);
    if rest != "." {
        return None;
    }
    Some(ParsedTriple {
        subject: s,
        predicate: p,
        object: o,
    })
}

fn skip_whitespace(s: &str) -> &str {
    s.trim_start()
}

fn parse_term(s: &str) -> Option<(ParsedTerm, &str)> {
    let s = skip_whitespace(s);
    if s.is_empty() {
        return None;
    }
    if s.starts_with('<') {
        parse_iri(s)
    } else if s.starts_with('"') {
        parse_literal(s)
    } else if s.starts_with("_:") {
        // Blank Nodes werden nicht unterstützt.
        None
    } else {
        None
    }
}

fn parse_iri(s: &str) -> Option<(ParsedTerm, &str)> {
    let end = s.find('>')?;
    let iri = s[1..end].to_string();
    Some((ParsedTerm { value: iri, typ: TermType::Iri }, &s[end + 1..]))
}

fn parse_literal(s: &str) -> Option<(ParsedTerm, &str)> {
    let (lexical, rest) = parse_quoted_string(s)?;
    let rest = skip_whitespace(rest);

    let (typ, rest) = if rest.starts_with('@') {
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let lang = rest[1..end].to_string();
        (TermType::literal_lang(lang), &rest[end..])
    } else if rest.starts_with("^^") {
        let rest = skip_whitespace(&rest[2..]);
        let (dt_term, rest) = parse_iri(rest)?;
        (TermType::literal_datatype(dt_term.value), rest)
    } else {
        (TermType::literal_plain(), rest)
    };

    Some((ParsedTerm { value: lexical, typ }, rest))
}

fn parse_quoted_string(s: &str) -> Option<(String, &str)> {
    // s beginnt mit "
    let mut result = String::new();
    let bytes = s.as_bytes();
    let mut i = 1; // nach dem öffnenden "

    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '"' => {
                return Some((result, &s[i + 1..]));
            }
            '\\' => {
                i += 1;
                if i >= bytes.len() {
                    return None;
                }
                let esc = bytes[i] as char;
                match esc {
                    '"' => result.push('"'),
                    '\\' => result.push('\\'),
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    'u' => {
                        if i + 4 >= bytes.len() {
                            return None;
                        }
                        let hex = std::str::from_utf8(&bytes[i + 1..i + 5]).ok()?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        result.push(char::from_u32(code)?);
                        i += 4;
                    }
                    'U' => {
                        if i + 8 >= bytes.len() {
                            return None;
                        }
                        let hex = std::str::from_utf8(&bytes[i + 1..i + 9]).ok()?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        result.push(char::from_u32(code)?);
                        i += 8;
                    }
                    _ => return None,
                }
            }
            _ => result.push(c),
        }
        i += 1;
    }

    None // nicht terminiert
}

/// Serialisiert einen Term (Wert + Typ) als gültige N-Triples-Syntax.
///
/// * IRI        → `<value>`
/// * Blank Node → `_:value`
/// * Literal    → `"escaped"`, `"escaped"@lang` oder `"escaped"^^<datatype>`
///
/// Gegenstück zu [`parse_term`]: `parse_term(serialize_term(v, t))` ist
/// (bis auf Whitespace) die Identität.
pub fn serialize_term(value: &str, typ: &TermType) -> String {
    match typ {
        TermType::Iri => format!("<{}>", value),
        TermType::BlankNode => format!("_:{}", value),
        TermType::Literal { datatype, lang } => {
            let escaped = escape_literal(value);
            if let Some(l) = lang {
                format!("\"{}\"@{}", escaped, l)
            } else if let Some(dt) = datatype {
                format!("\"{}\"^^<{}>", escaped, dt)
            } else {
                format!("\"{}\"", escaped)
            }
        }
    }
}

/// Escaped die Sonderzeichen eines Literal-Lexikals für N-Triples.
fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_iri_triple() {
        let line = "<http://example.org/s> <http://example.org/p> <http://example.org/o> .";
        let t = parse_triple_line(line).unwrap();
        assert_eq!(t.subject.value, "http://example.org/s");
        assert_eq!(t.predicate.value, "http://example.org/p");
        assert_eq!(t.object.value, "http://example.org/o");
        assert!(matches!(t.object.typ, TermType::Iri));
    }

    #[test]
    fn parses_literal_with_spaces() {
        let line = r#"<http://example.org/s> <http://example.org/p> "hello world" ."#;
        let t = parse_triple_line(line).unwrap();
        assert_eq!(t.object.value, "hello world");
        assert!(matches!(t.object.typ, TermType::Literal { datatype: None, lang: None }));
    }

    #[test]
    fn parses_literal_with_language_tag() {
        let line = r#"<http://example.org/s> <http://example.org/p> "hello"@en ."#;
        let t = parse_triple_line(line).unwrap();
        assert_eq!(t.object.value, "hello");
        assert!(matches!(t.object.typ, TermType::Literal { datatype: None, lang: Some(_) }));
    }

    #[test]
    fn parses_literal_with_datatype() {
        let line = r#"<http://example.org/s> <http://example.org/p> "30"^^<http://www.w3.org/2001/XMLSchema#integer> ."#;
        let t = parse_triple_line(line).unwrap();
        assert_eq!(t.object.value, "30");
        assert!(matches!(t.object.typ, TermType::Literal { datatype: Some(_), lang: None }));
    }

    #[test]
    fn parses_escaped_quotes() {
        let line = r#"<http://example.org/s> <http://example.org/p> "say \"hello\"" ."#;
        let t = parse_triple_line(line).unwrap();
        assert_eq!(t.object.value, r#"say "hello""#);
    }

    #[test]
    fn skips_blank_nodes() {
        let line = "<http://example.org/s> <http://example.org/p> _:b0 .";
        assert!(parse_triple_line(line).is_none());
    }
}
