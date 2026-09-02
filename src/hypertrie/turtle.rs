//! Turtle (`.ttl`) parser — [RDF 1.1 Turtle](https://www.w3.org/TR/turtle/),
//! minus RDF-star.
//!
//! Turtle is N-Triples plus the abbreviations that make it writable by hand:
//! prefixes, `a`, predicate-object and object lists, blank-node property lists,
//! collections, and the numeric/boolean literal shorthands. Every one of those
//! expands to plain triples, so the output is the same [`ParsedTriple`] the
//! N-Triples path produces and the ingest side stays unchanged.
//!
//! Unlike the N-Triples loader this is **not** streaming: a Turtle statement can
//! span any number of lines, so the document is parsed as a whole.

use std::collections::HashMap;

use super::dictionary::TermType;
use super::export::{ParsedTerm, ParsedTriple};

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// How deeply `[…]` and `(…)` may nest. Hand-written Turtle stays in the low
/// tens; the cap is what keeps a pathological document from recursing the
/// parser off the end of the stack.
const MAX_NESTING: usize = 128;

/// Parses a Turtle file into triples.
pub fn parse_turtle(path: &str) -> std::io::Result<Vec<ParsedTriple>> {
    let text = std::fs::read_to_string(path)?;
    parse_turtle_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{path}: {e}")))
}

/// Parses Turtle from a string. The error carries a line number.
pub fn parse_turtle_str(text: &str) -> Result<Vec<ParsedTriple>, String> {
    // A leading BOM is an encoding marker, not content — editors add one.
    let mut p = Parser::new(text.strip_prefix('\u{feff}').unwrap_or(text));
    p.document()?;
    Ok(p.out)
}

struct Parser<'a> {
    s: &'a str,
    /// Byte offset of the next unconsumed char.
    i: usize,
    base: Option<String>,
    prefixes: HashMap<String, String>,
    out: Vec<ParsedTriple>,
    /// Counter for the blank nodes `[]` and `(...)` generate.
    fresh: usize,
    /// Current `[…]`/`(…)` nesting, against [`MAX_NESTING`].
    depth: usize,
}

fn iri(value: impl Into<String>) -> ParsedTerm {
    ParsedTerm {
        value: value.into(),
        typ: TermType::Iri,
    }
}

fn blank(label: impl Into<String>) -> ParsedTerm {
    ParsedTerm {
        value: label.into(),
        typ: TermType::BlankNode,
    }
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Parser {
            s,
            i: 0,
            base: None,
            prefixes: HashMap::new(),
            out: Vec::new(),
            fresh: 0,
            depth: 0,
        }
    }

    /// Enters one `[…]`/`(…)` level. Errors abort the whole parse, so only the
    /// success paths need the matching [`Parser::leave`].
    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return self.err("nesting too deep");
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    // -- character-level helpers ------------------------------------------

    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, tok: &str) -> bool {
        if self.rest().starts_with(tok) {
            self.i += tok.len();
            true
        } else {
            false
        }
    }

    /// Same as [`Parser::eat`] but ASCII-case-insensitive, for the SPARQL-style
    /// `PREFIX`/`BASE` directives.
    fn eat_kw(&mut self, kw: &str) -> bool {
        // `get` rather than a slice: the cut may land inside a multi-byte char.
        if self
            .rest()
            .get(..kw.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(kw))
        {
            self.i += kw.len();
            true
        } else {
            false
        }
    }

    fn line(&self) -> usize {
        self.s[..self.i].bytes().filter(|&b| b == b'\n').count() + 1
    }

    fn err<T>(&self, msg: &str) -> Result<T, String> {
        Err(format!("line {}: {msg}", self.line()))
    }

    /// Skips whitespace and `#` comments. A `#` inside a quoted string never
    /// reaches here, because strings are consumed in one piece.
    fn ws(&mut self) {
        loop {
            let r = self.rest();
            let trimmed = r.trim_start();
            self.i += r.len() - trimmed.len();
            if self.peek() == Some('#') {
                let nl = self.rest().find('\n').unwrap_or(self.rest().len());
                self.i += nl;
            } else {
                return;
            }
        }
    }

    // -- document ---------------------------------------------------------

    fn document(&mut self) -> Result<(), String> {
        loop {
            self.ws();
            if self.rest().is_empty() {
                return Ok(());
            }
            self.statement()?;
        }
    }

    fn statement(&mut self) -> Result<(), String> {
        if self.eat("@prefix") {
            self.prefix_decl(true)
        } else if self.eat("@base") {
            self.base_decl(true)
        } else if self.eat_kw("PREFIX") {
            self.prefix_decl(false)
        } else if self.eat_kw("BASE") {
            self.base_decl(false)
        } else {
            self.triples()
        }
    }

    /// `@prefix p: <iri> .` — the SPARQL-style `PREFIX` form takes no final dot.
    fn prefix_decl(&mut self, dot: bool) -> Result<(), String> {
        self.ws();
        let Some(colon) = self.rest().find(':') else {
            return self.err("prefix declaration without ':'");
        };
        let name = self.rest()[..colon].to_string();
        // Reject here rather than at the use site, where the error is confusing.
        let legal = name.is_empty()
            || (name.starts_with(is_name_start_char) && name.chars().all(is_name_char));
        if !legal {
            return self.err("illegal prefix label");
        }
        self.i += colon + 1;
        self.ws();
        let Some(target) = self.iri_ref()? else {
            return self.err("prefix declaration without an IRI");
        };
        self.prefixes.insert(name, target);
        if dot {
            self.ws();
            if !self.eat(".") {
                return self.err("expected '.' after @prefix");
            }
        }
        Ok(())
    }

    fn base_decl(&mut self, dot: bool) -> Result<(), String> {
        self.ws();
        let Some(target) = self.iri_ref()? else {
            return self.err("base declaration without an IRI");
        };
        self.base = Some(target);
        if dot {
            self.ws();
            if !self.eat(".") {
                return self.err("expected '.' after @base");
            }
        }
        Ok(())
    }

    // -- triples ----------------------------------------------------------

    fn triples(&mut self) -> Result<(), String> {
        self.ws();
        // `[ ... ]` in subject position may stand alone as a whole statement.
        let subject = if self.peek() == Some('[') {
            let node = self.blank_node_property_list()?;
            self.ws();
            if self.eat(".") {
                return Ok(());
            }
            node
        } else if self.peek() == Some('(') {
            self.collection()?
        } else {
            match self.term()? {
                Some(t) => t,
                None => return self.err("expected a subject"),
            }
        };

        self.predicate_object_list(&subject)?;
        self.ws();
        if !self.eat(".") {
            return self.err("expected '.' at the end of the statement");
        }
        Ok(())
    }

    /// `p o (',' o)* (';' p o (',' o)*)*` — `;` repeats the subject, `,` repeats
    /// subject and predicate.
    fn predicate_object_list(&mut self, subject: &ParsedTerm) -> Result<(), String> {
        loop {
            self.ws();
            let predicate = if self.eat_verb_a() {
                iri(format!("{RDF}type"))
            } else {
                match self.term()? {
                    Some(t) => t,
                    None => return self.err("expected a predicate"),
                }
            };

            loop {
                self.ws();
                let object = self.object()?;
                self.out.push(ParsedTriple {
                    subject: subject.clone(),
                    predicate: predicate.clone(),
                    object,
                });
                self.ws();
                if !self.eat(",") {
                    break;
                }
            }

            self.ws();
            if !self.eat(";") {
                return Ok(());
            }
            // A trailing `;` before `.` or `]` is allowed.
            self.ws();
            if matches!(self.peek(), Some('.') | Some(']') | None) {
                return Ok(());
            }
        }
    }

    /// `a` is only the `rdf:type` keyword when it stands alone as a token.
    fn eat_verb_a(&mut self) -> bool {
        let r = self.rest();
        if let Some(after) = r.strip_prefix('a')
            && after
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == '<' || c == '[' || c == '"')
        {
            self.i += 1;
            return true;
        }
        false
    }

    fn object(&mut self) -> Result<ParsedTerm, String> {
        self.ws();
        match self.peek() {
            Some('[') => self.blank_node_property_list(),
            Some('(') => self.collection(),
            _ => match self.term()? {
                Some(t) => Ok(t),
                None => self.err("expected an object"),
            },
        }
    }

    /// `[ p o ; ... ]` — a fresh blank node carrying the nested statements.
    fn blank_node_property_list(&mut self) -> Result<ParsedTerm, String> {
        if !self.eat("[") {
            return self.err("expected '['");
        }
        self.enter()?;
        let node = self.fresh_blank();
        self.ws();
        if self.eat("]") {
            self.leave();
            return Ok(node); // `[]` — an anonymous blank node
        }
        self.predicate_object_list(&node)?;
        self.ws();
        if !self.eat("]") {
            return self.err("expected ']'");
        }
        self.leave();
        Ok(node)
    }

    /// `( a b c )` — an rdf:first/rdf:rest chain ending in rdf:nil.
    fn collection(&mut self) -> Result<ParsedTerm, String> {
        if !self.eat("(") {
            return self.err("expected '('");
        }
        self.enter()?;
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.eat(")") {
                break;
            }
            if self.rest().is_empty() {
                return self.err("unterminated collection");
            }
            items.push(self.object()?);
        }
        self.leave(); // past the closing ')', so past any nested item
        if items.is_empty() {
            return Ok(iri(format!("{RDF}nil")));
        }

        let head = self.fresh_blank();
        let mut cell = head.clone();
        let n = items.len();
        for (k, item) in items.into_iter().enumerate() {
            self.out.push(ParsedTriple {
                subject: cell.clone(),
                predicate: iri(format!("{RDF}first")),
                object: item,
            });
            let next = if k + 1 == n {
                iri(format!("{RDF}nil"))
            } else {
                self.fresh_blank()
            };
            self.out.push(ParsedTriple {
                subject: cell.clone(),
                predicate: iri(format!("{RDF}rest")),
                object: next.clone(),
            });
            cell = next;
        }
        Ok(head)
    }

    fn fresh_blank(&mut self) -> ParsedTerm {
        self.fresh += 1;
        blank(format!("_ttl{}", self.fresh))
    }

    /// Maps a blank node label written in the document into a space disjoint
    /// from the `_ttl<n>` labels [`Parser::fresh_blank`] mints, so `_:_ttl1` in
    /// the input cannot land on a node generated for a `[…]` or `(…)`.
    /// Escaping only the leading `_` keeps ordinary labels verbatim and stays
    /// injective: `_x` and `__x` remain distinct nodes.
    fn document_label(&self, label: &str) -> String {
        match label.starts_with('_') {
            true => format!("_{label}"),
            false => label.to_string(),
        }
    }

    // -- terms ------------------------------------------------------------

    /// One IRI, blank node or literal. `None` when the next char starts none of
    /// them (so the caller can report where it happened).
    fn term(&mut self) -> Result<Option<ParsedTerm>, String> {
        self.ws();
        match self.peek() {
            None => Ok(None),
            Some('<') => Ok(self.iri_ref()?.map(iri)),
            Some('"') | Some('\'') => self.literal().map(Some),
            Some('_') if self.rest().starts_with("_:") => {
                self.i += 2;
                let mut label = self.take_while(is_name_char);
                self.give_back_trailing_dots(&mut label);
                if label.is_empty() {
                    return self.err("blank node without a label");
                }
                Ok(Some(blank(self.document_label(&label))))
            }
            Some(c) if c.is_ascii_digit() || c == '+' || c == '-' || c == '.' => {
                self.numeric_literal().map(Some)
            }
            Some(_) => {
                if self.rest().starts_with("true") {
                    self.i += 4;
                    return Ok(Some(typed("true", "boolean")));
                }
                if self.rest().starts_with("false") {
                    self.i += 5;
                    return Ok(Some(typed("false", "boolean")));
                }
                self.prefixed_name().map(|o| o.map(iri))
            }
        }
    }

    /// `<iri>`, resolved against `@base` when relative.
    fn iri_ref(&mut self) -> Result<Option<String>, String> {
        if !self.eat("<") {
            return Ok(None);
        }
        let mut raw = String::new();
        loop {
            match self.bump() {
                None => return self.err("unterminated IRI"),
                Some('>') => break,
                // An IRI may carry \uXXXX escapes just as a literal may.
                Some('\\') => match self.bump() {
                    Some('u') => raw.push(self.hex(4)?),
                    Some('U') => raw.push(self.hex(8)?),
                    _ => return self.err("bad escape in IRI"),
                },
                Some(c) => raw.push(c),
            }
        }
        Ok(Some(self.resolve(raw)))
    }

    /// `prefix:local`, `:local` — and the local part may escape reserved
    /// characters with a backslash (`ex:a\-b`).
    fn prefixed_name(&mut self) -> Result<Option<String>, String> {
        let start = self.i;
        let mut prefix = self.take_while(is_name_char);
        self.give_back_trailing_dots(&mut prefix);
        // The empty prefix is legal; a non-empty one must start with PN_CHARS_BASE.
        if !prefix.is_empty() && !prefix.starts_with(is_name_start_char) {
            self.i = start;
            return self.err("prefix label starts with an illegal character");
        }
        if !self.eat(":") {
            self.i = start;
            return self.err("expected a term");
        }
        let mut local = String::new();
        // Last position at which the local part may legally stop. An escaped
        // `\.` is a valid ending, a bare `.` is not, so the two cannot share
        // `give_back_trailing_dots`.
        let (mut end_i, mut end_len) = (self.i, 0usize);
        loop {
            match self.peek() {
                Some('\\') => {
                    self.bump();
                    match self.bump() {
                        Some(c) if is_local_escapable(c) => local.push(c),
                        _ => return self.err("bad escape in a prefixed name"),
                    }
                    (end_i, end_len) = (self.i, local.len());
                }
                Some(c) if is_local_char(c) => {
                    local.push(c);
                    self.bump();
                    if c != '.' {
                        (end_i, end_len) = (self.i, local.len());
                    }
                }
                _ => break,
            }
        }
        self.i = end_i;
        local.truncate(end_len);
        match self.prefixes.get(&prefix) {
            Some(ns) => Ok(Some(format!("{ns}{local}"))),
            None => self.err(&format!("unknown prefix '{prefix}:'")),
        }
    }

    /// Integers, decimals and doubles get their xsd datatype from their shape,
    /// exactly as Turtle defines.
    fn numeric_literal(&mut self) -> Result<ParsedTerm, String> {
        let start = self.i;
        if matches!(self.peek(), Some('+') | Some('-')) {
            self.bump();
        }
        let mut is_decimal = false;
        let mut is_double = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.bump();
            } else if c == '.' {
                // Only a digit after the dot makes it part of the number;
                // otherwise it is the statement-ending dot.
                let after = &self.rest()[1..];
                if after.chars().next().is_some_and(|d| d.is_ascii_digit()) {
                    is_decimal = true;
                    self.bump();
                } else {
                    break;
                }
            } else if c == 'e' || c == 'E' {
                is_double = true;
                self.bump();
                if matches!(self.peek(), Some('+') | Some('-')) {
                    self.bump();
                }
            } else {
                break;
            }
        }
        let lex = &self.s[start..self.i];
        if lex.is_empty() || lex == "+" || lex == "-" {
            return self.err("expected a number");
        }
        let dt = if is_double {
            "double"
        } else if is_decimal {
            "decimal"
        } else {
            "integer"
        };
        Ok(typed(lex, dt))
    }

    /// A quoted string plus an optional `@lang` or `^^<datatype>`. All four
    /// Turtle quotings are accepted: `"`, `'`, `"""` and `'''`.
    fn literal(&mut self) -> Result<ParsedTerm, String> {
        let lexical = self.quoted_string()?;
        if self.eat("@") {
            let lang = self.take_while(|c| c.is_ascii_alphanumeric() || c == '-');
            if lang.is_empty() {
                return self.err("empty language tag");
            }
            return Ok(ParsedTerm {
                value: lexical,
                typ: TermType::literal_lang(lang),
            });
        }
        if self.eat("^^") {
            self.ws();
            let dt = match self.peek() {
                Some('<') => match self.iri_ref()? {
                    Some(v) => v,
                    None => return self.err("expected a datatype IRI"),
                },
                _ => match self.prefixed_name()? {
                    Some(v) => v,
                    None => return self.err("expected a datatype IRI"),
                },
            };
            return Ok(ParsedTerm {
                value: lexical,
                typ: TermType::literal_datatype(dt),
            });
        }
        Ok(ParsedTerm {
            value: lexical,
            typ: TermType::literal_plain(),
        })
    }

    fn quoted_string(&mut self) -> Result<String, String> {
        let (delim, long) = if self.eat("\"\"\"") {
            ("\"\"\"", true)
        } else if self.eat("'''") {
            ("'''", true)
        } else if self.eat("\"") {
            ("\"", false)
        } else if self.eat("'") {
            ("'", false)
        } else {
            return self.err("expected a quoted string");
        };

        let mut out = String::new();
        loop {
            if self.eat(delim) {
                return Ok(out);
            }
            match self.bump() {
                None => return self.err("unterminated string"),
                Some('\\') => {
                    let Some(esc) = self.bump() else {
                        return self.err("unterminated escape");
                    };
                    out.push(match esc {
                        '"' => '"',
                        '\'' => '\'',
                        '\\' => '\\',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'b' => '\u{8}',
                        'f' => '\u{c}',
                        'u' => self.hex(4)?,
                        'U' => self.hex(8)?,
                        _ => return self.err("unknown escape"),
                    });
                }
                // A raw newline only belongs to the long forms.
                Some(c) if (c == '\n' || c == '\r') && !long => {
                    return self.err("newline in a single-quoted string");
                }
                Some(c) => out.push(c),
            }
        }
    }

    fn hex(&mut self, n: usize) -> Result<char, String> {
        let mut code: u32 = 0;
        for _ in 0..n {
            let Some(c) = self.bump() else {
                return self.err("truncated \\u escape");
            };
            let Some(d) = c.to_digit(16) else {
                return self.err("non-hex digit in \\u escape");
            };
            code = code * 16 + d;
        }
        match char::from_u32(code) {
            Some(c) => Ok(c),
            None => self.err("\\u escape is not a codepoint"),
        }
    }

    fn take_while(&mut self, f: impl Fn(char) -> bool) -> String {
        let start = self.i;
        while self.peek().is_some_and(&f) {
            self.bump();
        }
        self.s[start..self.i].to_string()
    }

    /// A Turtle name may contain a `.` but must not end with one — a trailing
    /// dot belongs to the statement (`ex:a ex:b ex:c.`), not to the name.
    fn give_back_trailing_dots(&mut self, name: &mut String) {
        while name.ends_with('.') {
            name.pop();
            self.i -= 1; // '.' is one byte
        }
    }

    /// Resolves a relative reference against `@base`. Enough for the forms
    /// Turtle documents actually use: absolute, empty, fragment, absolute path,
    /// and a plain relative path.
    fn resolve(&self, reference: String) -> String {
        let Some(base) = &self.base else {
            return reference;
        };
        if reference.is_empty() {
            return base.clone();
        }
        if reference.contains("://") || reference.starts_with("urn:") {
            return reference;
        }
        if let Some(frag) = reference.strip_prefix('#') {
            let stem = base.split('#').next().unwrap_or(base);
            return format!("{stem}#{frag}");
        }
        if let Some(abs) = reference.strip_prefix('/') {
            // Keep scheme + authority, replace the path.
            let after_scheme = base.find("://").map(|k| k + 3).unwrap_or(0);
            let end = base[after_scheme..]
                .find('/')
                .map(|k| after_scheme + k)
                .unwrap_or(base.len());
            return format!("{}/{abs}", &base[..end]);
        }
        let stem = match base.rfind('/') {
            Some(k) => &base[..=k],
            None => base.as_str(),
        };
        format!("{stem}{reference}")
    }
}

fn typed(lexical: &str, xsd_suffix: &str) -> ParsedTerm {
    ParsedTerm {
        value: lexical.to_string(),
        typ: TermType::literal_datatype(format!("{XSD}{xsd_suffix}")),
    }
}

/// Characters allowed in a prefix label or blank-node label.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || !c.is_ascii()
}

/// Turtle's `PN_CHARS_BASE`: what a prefix label may start with. Narrower than
/// [`is_name_char`], which also has to cover blank-node labels (`_:1`).
fn is_name_start_char(c: char) -> bool {
    c.is_alphabetic() || !c.is_ascii()
}

/// Characters allowed unescaped in the local part of a prefixed name.
fn is_local_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '%' | ':') || !c.is_ascii()
}

/// The reserved characters a local name may escape with a backslash.
fn is_local_escapable(c: char) -> bool {
    matches!(
        c,
        '_' | '~'
            | '.'
            | '-'
            | '!'
            | '$'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | ';'
            | '='
            | '/'
            | '?'
            | '#'
            | '@'
            | '%'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Triples as `subject|predicate|object` for order-independent comparison.
    fn parse(ttl: &str) -> Vec<String> {
        parse_turtle_str(ttl)
            .unwrap_or_else(|e| panic!("parse failed: {e}\n---\n{ttl}"))
            .iter()
            .map(|t| {
                format!(
                    "{}|{}|{}",
                    t.subject.value, t.predicate.value, t.object.value
                )
            })
            .collect()
    }

    const PRE: &str = "@prefix ex: <http://example.org/> .\n";

    #[test]
    fn prefixed_names_and_a_keyword() {
        let got = parse(&format!("{PRE}ex:alice a ex:Person ."));
        assert_eq!(
            got,
            [
                "http://example.org/alice|http://www.w3.org/1999/02/22-rdf-syntax-ns#type|http://example.org/Person"
            ]
        );
    }

    #[test]
    fn sparql_style_prefix_without_dot() {
        let got = parse("PREFIX ex: <http://example.org/>\nex:a ex:b ex:c .");
        assert_eq!(
            got,
            ["http://example.org/a|http://example.org/b|http://example.org/c"]
        );
    }

    #[test]
    fn empty_prefix() {
        let got = parse("@prefix : <http://example.org/> .\n:a :b :c .");
        assert_eq!(
            got,
            ["http://example.org/a|http://example.org/b|http://example.org/c"]
        );
    }

    #[test]
    fn a_prefix_label_must_start_with_a_letter() {
        let e = parse_turtle_str("@prefix _x: <http://example.org/> .\n_x:a _x:b _x:c .")
            .expect_err("'_' is not PN_CHARS_BASE");
        assert!(e.contains("illegal prefix label"), "got {e}");
    }

    #[test]
    fn predicate_object_and_object_lists() {
        // `;` repeats the subject, `,` repeats subject and predicate.
        let got = parse(&format!("{PRE}ex:a ex:knows ex:b, ex:c ; ex:age 30 ."));
        assert_eq!(
            got,
            [
                "http://example.org/a|http://example.org/knows|http://example.org/b",
                "http://example.org/a|http://example.org/knows|http://example.org/c",
                "http://example.org/a|http://example.org/age|30",
            ]
        );
    }

    #[test]
    fn trailing_semicolon_is_allowed() {
        let got = parse(&format!("{PRE}ex:a ex:b ex:c ; ."));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn numeric_and_boolean_literals_get_their_datatype() {
        let ttl = format!("{PRE}ex:a ex:i 42 ; ex:d 3.14 ; ex:e 1.0e6 ; ex:n -7 ; ex:b true .");
        let ts = parse_turtle_str(&ttl).unwrap();
        let dt = |k: usize| match &ts[k].object.typ {
            TermType::Literal {
                datatype: Some(d), ..
            } => d.clone(),
            other => panic!("expected a typed literal, got {other:?}"),
        };
        assert_eq!(dt(0), format!("{XSD}integer"));
        assert_eq!(dt(1), format!("{XSD}decimal"));
        assert_eq!(dt(2), format!("{XSD}double"));
        assert_eq!(dt(3), format!("{XSD}integer"));
        assert_eq!(ts[3].object.value, "-7");
        assert_eq!(dt(4), format!("{XSD}boolean"));
    }

    #[test]
    fn integer_followed_by_statement_dot() {
        // The dot after `42` ends the statement; it is not a decimal point.
        let got = parse(&format!("{PRE}ex:a ex:n 42.\nex:b ex:n 43 ."));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], "http://example.org/a|http://example.org/n|42");
    }

    #[test]
    fn blank_node_property_list() {
        let ts = parse_turtle_str(&format!(
            "{PRE}ex:a ex:knows [ ex:name \"Bob\" ; ex:age 30 ] ."
        ))
        .unwrap();
        assert_eq!(ts.len(), 3);
        // The bnode is the object of ex:knows and the subject of the nested two.
        let outer = ts
            .iter()
            .find(|t| t.predicate.value.ends_with("knows"))
            .unwrap();
        assert_eq!(outer.object.typ, TermType::BlankNode);
        let nested: Vec<_> = ts
            .iter()
            .filter(|t| t.subject.value == outer.object.value)
            .collect();
        assert_eq!(nested.len(), 2);
    }

    #[test]
    fn anonymous_blank_node() {
        let ts = parse_turtle_str(&format!("{PRE}ex:a ex:knows [] .")).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].object.typ, TermType::BlankNode);
    }

    #[test]
    fn blank_node_property_list_as_subject() {
        let ts = parse_turtle_str(&format!("{PRE}[ ex:name \"anon\" ] ex:age 1 .")).unwrap();
        assert_eq!(ts.len(), 2);
        assert_eq!(ts[0].subject.typ, TermType::BlankNode);
        assert_eq!(ts[0].subject.value, ts[1].subject.value);
    }

    #[test]
    fn collection_expands_to_a_first_rest_chain() {
        let ts = parse_turtle_str(&format!("{PRE}ex:a ex:list ( ex:x ex:y ) .")).unwrap();
        // ex:list + 2x (first, rest) = 5 triples, ending in rdf:nil.
        assert_eq!(ts.len(), 5);
        let firsts: Vec<_> = ts
            .iter()
            .filter(|t| t.predicate.value == format!("{RDF}first"))
            .collect();
        assert_eq!(firsts.len(), 2);
        assert_eq!(firsts[0].object.value, "http://example.org/x");
        assert_eq!(firsts[1].object.value, "http://example.org/y");
        assert!(ts.iter().any(|t| t.object.value == format!("{RDF}nil")));
    }

    #[test]
    fn empty_collection_is_nil() {
        let ts = parse_turtle_str(&format!("{PRE}ex:a ex:list () .")).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].object.value, format!("{RDF}nil"));
    }

    #[test]
    fn long_strings_and_quote_forms() {
        let ttl = format!(
            "{PRE}ex:a ex:s1 \"\"\"line1\nline2\"\"\" ; ex:s2 'single' ; ex:s3 '''tri\nple''' ."
        );
        let ts = parse_turtle_str(&ttl).unwrap();
        assert_eq!(ts[0].object.value, "line1\nline2");
        assert_eq!(ts[1].object.value, "single");
        assert_eq!(ts[2].object.value, "tri\nple");
    }

    #[test]
    fn non_ascii_and_escapes_in_literals() {
        let ttl = format!("{PRE}ex:a ex:name \"Grüße 🎉\" ; ex:esc \"q\\\"\\n\\u00e9\" .");
        let ts = parse_turtle_str(&ttl).unwrap();
        assert_eq!(ts[0].object.value, "Grüße 🎉");
        assert_eq!(ts[1].object.value, "q\"\n\u{e9}");
    }

    #[test]
    fn language_tags_and_explicit_datatypes() {
        let ttl = format!(
            "{PRE}ex:a ex:n \"hallo\"@de ; ex:m \"5\"^^<{XSD}integer> ; ex:o \"6\"^^ex:custom ."
        );
        let ts = parse_turtle_str(&ttl).unwrap();
        assert!(matches!(&ts[0].object.typ, TermType::Literal { lang: Some(l), .. } if l == "de"));
        assert!(
            matches!(&ts[1].object.typ, TermType::Literal { datatype: Some(d), .. } if *d == format!("{XSD}integer"))
        );
        assert!(
            matches!(&ts[2].object.typ, TermType::Literal { datatype: Some(d), .. } if d == "http://example.org/custom")
        );
    }

    #[test]
    fn comments_are_skipped_but_not_inside_strings() {
        let ttl =
            format!("# leading comment\n{PRE}ex:a ex:b \"text # not a comment\" . # trailing\n");
        let ts = parse_turtle_str(&ttl).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].object.value, "text # not a comment");
    }

    #[test]
    fn statements_may_span_lines() {
        let got = parse(&format!("{PRE}ex:a\n  ex:knows\n    ex:b\n  .\n"));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn base_resolves_relative_iris() {
        let ttl = "@base <http://example.org/dir/page> .\n<#frag> <p> <sub> .\n<//x> <q> </abs> .";
        let ts = parse_turtle_str(ttl).unwrap();
        assert_eq!(ts[0].subject.value, "http://example.org/dir/page#frag");
        assert_eq!(ts[0].predicate.value, "http://example.org/dir/p");
        assert_eq!(ts[0].object.value, "http://example.org/dir/sub");
        assert_eq!(ts[1].object.value, "http://example.org/abs");
    }

    #[test]
    fn blank_node_labels_are_kept() {
        let ts = parse_turtle_str(&format!("{PRE}_:x ex:p _:y . _:x ex:q 1 .")).unwrap();
        assert_eq!(ts[0].subject.typ, TermType::BlankNode);
        assert_eq!(ts[0].subject.value, "x");
        assert_eq!(ts[0].object.value, "y");
        // The same label is the same node across statements.
        assert_eq!(ts[0].subject.value, ts[1].subject.value);
    }

    #[test]
    fn a_generated_blank_node_cannot_be_hijacked_by_a_document_label() {
        // `_ttl1` is what `[ ... ]` mints here, and `_:_ttl1` is a legal label,
        // so the two must still end up as different nodes.
        let ts = parse_turtle_str(&format!(
            "{PRE}ex:a ex:b [ ex:c ex:d ] .\n_:_ttl1 ex:e ex:f ."
        ))
        .unwrap();
        let generated = &ts[0].subject.value;
        let written = &ts[2].subject.value;
        assert_eq!(ts[2].subject.typ, TermType::BlankNode);
        assert_ne!(generated, written);
    }

    #[test]
    fn escaping_document_labels_stays_injective() {
        let ts = parse_turtle_str(&format!("{PRE}ex:a ex:b _:_x .\nex:a ex:c _:__x .")).unwrap();
        assert_ne!(ts[0].object.value, ts[1].object.value);
    }

    #[test]
    fn a_name_may_contain_a_dot_but_not_end_with_one() {
        // The dot after `ex:c` ends the statement; the one in `ex:v1.2` does not.
        let got = parse(&format!("{PRE}ex:v1.2 ex:b ex:c.\nex:d ex:e _:x."));
        assert_eq!(
            got,
            [
                "http://example.org/v1.2|http://example.org/b|http://example.org/c",
                "http://example.org/d|http://example.org/e|x",
            ]
        );
    }

    #[test]
    fn an_escaped_dot_may_end_a_local_name() {
        let got = parse(&format!("{PRE}ex:a ex:b ex:c\\.."));
        assert_eq!(
            got,
            ["http://example.org/a|http://example.org/b|http://example.org/c."]
        );
    }

    #[test]
    fn a_multi_byte_char_at_a_keyword_boundary_is_an_error_not_a_panic() {
        // `basé`/`prefiä` put a multi-byte char across the `BASE`/`PREFIX`
        // comparison window; slicing there used to panic.
        for doc in [
            "basé",
            "prefiä",
            &format!("{PRE}basé:a ex:b ex:c ."),
            "🎉x ex:p ex:o .",
        ] {
            assert!(
                parse_turtle_str(doc).is_err(),
                "expected an error for {doc:?}"
            );
        }
    }

    #[test]
    fn a_leading_bom_is_not_content() {
        let got = parse(&format!("\u{feff}{PRE}ex:a ex:b ex:c ."));
        assert_eq!(
            got,
            ["http://example.org/a|http://example.org/b|http://example.org/c"]
        );
    }

    #[test]
    fn nesting_past_the_cap_is_an_error_not_a_stack_overflow() {
        for (open, close) in [("[ ex:p ", " ]"), ("( ", " )")] {
            let deep = format!(
                "{PRE}ex:a ex:b {}ex:end{} .",
                open.repeat(MAX_NESTING + 1),
                close.repeat(MAX_NESTING + 1)
            );
            let e = parse_turtle_str(&deep).unwrap_err();
            assert!(e.contains("nesting too deep"), "got {e}");
        }
    }

    #[test]
    fn nesting_within_the_cap_still_parses() {
        let n = MAX_NESTING - 1;
        let deep = format!(
            "{PRE}ex:a ex:b {}ex:end{} .",
            "[ ex:p ".repeat(n),
            " ]".repeat(n)
        );
        assert_eq!(parse_turtle_str(&deep).unwrap().len(), n + 1);
    }

    #[test]
    fn the_depth_counter_unwinds_between_siblings() {
        // Sequential (not nested) brackets must not accumulate depth.
        let one = "ex:a ex:b [ ex:p ex:q ] .\n".repeat(MAX_NESTING * 3);
        assert!(parse_turtle_str(&format!("{PRE}{one}")).is_ok());
        let lists = "ex:a ex:b ( ex:x ) .\n".repeat(MAX_NESTING * 3);
        assert!(parse_turtle_str(&format!("{PRE}{lists}")).is_ok());
    }

    #[test]
    fn escaped_local_name() {
        let got = parse(&format!("{PRE}ex:a\\-b ex:p ex:c ."));
        assert_eq!(
            got[0],
            "http://example.org/a-b|http://example.org/p|http://example.org/c"
        );
    }

    #[test]
    fn errors_carry_a_line_number() {
        let e = parse_turtle_str(
            "@prefix ex: <http://example.org/> .\nex:a ex:b ex:c\nex:d ex:e ex:f .",
        )
        .unwrap_err();
        assert!(e.starts_with("line "), "expected a line number, got {e}");
    }

    #[test]
    fn unknown_prefix_is_an_error() {
        let e = parse_turtle_str("nope:a nope:b nope:c .").unwrap_err();
        assert!(e.contains("unknown prefix"), "got {e}");
    }

    #[test]
    fn unterminated_string_is_an_error() {
        assert!(parse_turtle_str(&format!("{PRE}ex:a ex:b \"oops .")).is_err());
    }
}
