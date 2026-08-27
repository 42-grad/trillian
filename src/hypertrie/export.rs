use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};

use super::dictionary::TermType;

/// Exports string triples in N-Triples format (.nt).
///
/// Each line has the form:
/// `<http://example.org/subject> <http://example.org/predicate> <http://example.org/object> .`
///
/// All terms are treated as URIs; literals are not quoted separately
/// (sufficient for the benchmark suite).
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

#[derive(Debug, Clone)]
pub struct ParsedTerm {
    pub value: String,
    pub typ: TermType,
}

#[derive(Debug, Clone)]
pub struct ParsedTriple {
    pub subject: ParsedTerm,
    pub predicate: ParsedTerm,
    pub object: ParsedTerm,
}

/// Parses an N-Triples file (.nt) and returns (subject, predicate, object) as
/// full term strings including the term type.
///
/// Supports:
///   - IRIs: `<http://example.org/s>`
///   - Literals: `"hello"`, `"hello"@en`, `"30"^^<http://www.w3.org/2001/XMLSchema#integer>`
///   - escape sequences in strings: `\"`, `\'`, `\\`, `\n`, `\r`, `\t`, `\b`, `\f`,
///     `\uXXXX`, `\UXXXXXXXX`
///
/// Blank nodes (`_:b0`) are parsed as [`TermType::BlankNode`] (document-scoped).
/// Syntactically invalid lines are skipped.
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
        parse_blank_node(s)
    } else {
        None
    }
}

/// Parses a blank node `_:label`. The label is document-scoped; for a
/// single-file ingest it serves directly as the dictionary key (type
/// [`TermType::BlankNode`]), so the same `_:label` maps to the same ID across
/// all lines. Value without the `_:` prefix (serialization in
/// [`serialize_term`] puts it back).
fn parse_blank_node(s: &str) -> Option<(ParsedTerm, &str)> {
    let after = &s[2..]; // after "_:"
    let end = after
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after.len());
    if end == 0 {
        return None;
    }
    Some((
        ParsedTerm {
            value: after[..end].to_string(),
            typ: TermType::BlankNode,
        },
        &after[end..],
    ))
}

fn parse_iri(s: &str) -> Option<(ParsedTerm, &str)> {
    let end = s.find('>')?;
    let iri = s[1..end].to_string();
    Some((
        ParsedTerm {
            value: iri,
            typ: TermType::Iri,
        },
        &s[end + 1..],
    ))
}

fn parse_literal(s: &str) -> Option<(ParsedTerm, &str)> {
    let (lexical, rest) = parse_quoted_string(s)?;
    let rest = skip_whitespace(rest);

    let (typ, rest) = if rest.starts_with('@') {
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let lang = rest[1..end].to_string();
        (TermType::literal_lang(lang), &rest[end..])
    } else if let Some(stripped) = rest.strip_prefix("^^") {
        let rest = skip_whitespace(stripped);
        let (dt_term, rest) = parse_iri(rest)?;
        (TermType::literal_datatype(dt_term.value), rest)
    } else {
        (TermType::literal_plain(), rest)
    };

    Some((
        ParsedTerm {
            value: lexical,
            typ,
        },
        rest,
    ))
}

/// Reads a quoted string starting at the opening `"`, resolving escapes.
///
/// Iterates over `char`s, not bytes: a raw UTF-8 byte reinterpreted as a
/// `char` is a Latin-1 codepoint, which silently mangles every non-ASCII
/// literal ("café" -> "cafÃ©").
fn parse_quoted_string(s: &str) -> Option<(String, &str)> {
    let mut result = String::new();
    let mut it = s.char_indices();
    it.next()?; // opening quote
    while let Some((i, c)) = it.next() {
        match c {
            '"' => return Some((result, &s[i + 1..])),
            '\\' => {
                let (_, esc) = it.next()?;
                result.push(match esc {
                    '"' => '"',
                    '\'' => '\'',
                    '\\' => '\\',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'u' => take_hex(&mut it, 4)?,
                    'U' => take_hex(&mut it, 8)?,
                    _ => return None,
                });
            }
            _ => result.push(c),
        }
    }
    None // not terminated
}

/// Reads exactly `n` hex digits as one codepoint (`\uXXXX` / `\UXXXXXXXX`).
fn take_hex(it: &mut std::str::CharIndices, n: usize) -> Option<char> {
    let mut code: u32 = 0;
    for _ in 0..n {
        code = code
            .checked_mul(16)?
            .checked_add(it.next()?.1.to_digit(16)?)?;
    }
    char::from_u32(code)
}

/// Serializes a term (value + type) as valid N-Triples syntax.
///
/// * IRI        → `<value>`
/// * Blank node → `_:value`
/// * Literal    → `"escaped"`, `"escaped"@lang` or `"escaped"^^<datatype>`
///
/// Counterpart to [`parse_term`]: `parse_term(serialize_term(v, t))` is the
/// identity (up to whitespace).
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

/// Escapes the special characters of a literal lexical form for N-Triples.
fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
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
        assert!(matches!(
            t.object.typ,
            TermType::Literal {
                datatype: None,
                lang: None
            }
        ));
    }

    #[test]
    fn parses_literal_with_language_tag() {
        let line = r#"<http://example.org/s> <http://example.org/p> "hello"@en ."#;
        let t = parse_triple_line(line).unwrap();
        assert_eq!(t.object.value, "hello");
        assert!(matches!(
            t.object.typ,
            TermType::Literal {
                datatype: None,
                lang: Some(_)
            }
        ));
    }

    #[test]
    fn parses_literal_with_datatype() {
        let line = r#"<http://example.org/s> <http://example.org/p> "30"^^<http://www.w3.org/2001/XMLSchema#integer> ."#;
        let t = parse_triple_line(line).unwrap();
        assert_eq!(t.object.value, "30");
        assert!(matches!(
            t.object.typ,
            TermType::Literal {
                datatype: Some(_),
                lang: None
            }
        ));
    }

    #[test]
    fn parses_escaped_quotes() {
        let line = r#"<http://example.org/s> <http://example.org/p> "say \"hello\"" ."#;
        let t = parse_triple_line(line).unwrap();
        assert_eq!(t.object.value, r#"say "hello""#);
    }

    #[test]
    fn parses_non_ascii_literals_verbatim() {
        // Byte-wise parsing reinterpreted each UTF-8 byte as a Latin-1
        // codepoint, so every non-ASCII literal came back mangled.
        for lit in ["café", "日本語", "naïve", "Grüße", "emoji 🎉", "Ωμέγα"] {
            let line = format!("<http://example.org/s> <http://example.org/p> \"{lit}\" .");
            let t = parse_triple_line(&line).expect("must parse");
            assert_eq!(t.object.value, lit, "literal must survive verbatim");
        }
    }

    #[test]
    fn parses_escapes_next_to_non_ascii() {
        // The escape branch also advanced by bytes, so a multi-byte char right
        // after an escape used to desynchronize the scan.
        let line = r#"<http://example.org/s> <http://example.org/p> "sagt \"Grüße\"\nüber" ."#;
        let t = parse_triple_line(line).unwrap();
        assert_eq!(t.object.value, "sagt \"Grüße\"\nüber");
    }

    #[test]
    fn parses_the_full_escape_set() {
        let line = r#"<http://example.org/s> <http://example.org/p> "q\" a\' b\\ n\n r\r t\t b\b f\f u\u00e9 U\U0001F389" ."#;
        let t = parse_triple_line(line).unwrap();
        assert_eq!(
            t.object.value,
            "q\" a' b\\ n\n r\r t\t b\u{8} f\u{c} u\u{e9} U\u{1F389}"
        );
    }

    #[test]
    fn non_ascii_literal_round_trips_through_serialization() {
        let line = "<http://example.org/s> <http://example.org/p> \"Grüße 🎉\tende\" .";
        let t = parse_triple_line(line).unwrap();
        let out = serialize_term(&t.object.value, &t.object.typ);
        let back = parse_triple_line(&format!(
            "<http://example.org/s> <http://example.org/p> {out} ."
        ))
        .unwrap();
        assert_eq!(back.object.value, t.object.value);
    }

    #[test]
    fn rejects_a_truncated_escape() {
        let line = r#"<http://example.org/s> <http://example.org/p> "bad \u00" ."#;
        assert!(parse_triple_line(line).is_none());
    }

    #[test]
    fn parses_blank_nodes() {
        // Object blank node.
        let t = parse_triple_line("<http://example.org/s> <http://example.org/p> _:b0 .")
            .expect("triple with blank-node object must parse");
        assert_eq!(t.object.typ, TermType::BlankNode);
        assert_eq!(t.object.value, "b0"); // without _: prefix

        // Subject blank node + round-trip through serialization.
        let t2 = parse_triple_line("_:n1 <http://example.org/p> <http://example.org/o> .")
            .expect("triple with blank-node subject must parse");
        assert_eq!(t2.subject.typ, TermType::BlankNode);
        assert_eq!(serialize_term(&t2.subject.value, &t2.subject.typ), "_:n1");
    }
}
