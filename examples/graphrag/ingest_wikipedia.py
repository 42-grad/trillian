#!/usr/bin/env python3
"""
ingest_wikipedia.py — build an RDF knowledge graph from a Wikipedia article
using an LLM to extract (subject, predicate, object) triples, then write them
as N-Triples that Trillian can load.

This is the *ingestion* half of GraphRAG: turn unstructured prose into a graph.
The companion `graphrag.py` then answers questions by walking that graph.

Pipeline:
  1. Fetch the article's plain text from the Wikipedia API (no key needed).
  2. Split it into chunks and ask Mistral to extract factual triples.
  3. Normalize entities/predicates to IRIs and emit N-Triples (+ rdfs:label).

Usage:
  export MISTRAL_API_KEY=...
  python ingest_wikipedia.py "The Hitchhiker's Guide to the Galaxy" --out hitchhikers.nt

Requires a network connection and MISTRAL_API_KEY. The repository already ships
a pre-generated `hitchhikers.nt`, so you can try `graphrag.py` without running
this step.
"""

import argparse
import json
import os
import re
import sys
import urllib.parse
import urllib.request

ENT = "http://example.org/hhg/entity/"
PROP = "http://example.org/hhg/prop/"
RDFS_LABEL = "http://www.w3.org/2000/01/rdf-schema#label"
XSD_INT = "http://www.w3.org/2001/XMLSchema#integer"

WIKI_API = "https://en.wikipedia.org/w/api.php"


def fetch_extract(title: str) -> str:
    """Fetch the plain-text extract of a Wikipedia article."""
    params = {
        "action": "query",
        "prop": "extracts",
        "explaintext": "1",
        "redirects": "1",
        "format": "json",
        "titles": title,
    }
    url = f"{WIKI_API}?{urllib.parse.urlencode(params)}"
    req = urllib.request.Request(url, headers={"User-Agent": "trillian-graphrag-example/1.0"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        data = json.load(resp)
    pages = data.get("query", {}).get("pages", {})
    page = next(iter(pages.values()), {})
    text = page.get("extract")
    if not text:
        sys.exit(f"No extract found for {title!r} (page missing?)")
    return text


def chunks(text: str, size: int):
    """Group paragraphs into ~`size`-character chunks (keeps prompts small)."""
    buf = ""
    for para in (p.strip() for p in text.split("\n")):
        if not para or para.startswith("=="):  # skip blanks and section headers
            continue
        if len(buf) + len(para) > size and buf:
            yield buf
            buf = ""
        buf += para + "\n"
    if buf:
        yield buf


EXTRACT_PROMPT = (
    "Extract factual knowledge-graph triples from the text below. "
    "Return ONLY a JSON array of objects with keys "
    '"subject", "predicate", "object", and "object_is_entity" (boolean: true if '
    "the object is a named thing/person/place, false if it is a literal value "
    "like a number, date, or free text). Use short, canonical names. "
    "Example: "
    '[{"subject":"Douglas Adams","predicate":"wrote","object":"The Hitchhikers Guide to the Galaxy","object_is_entity":true}]\n\n'
    "Text:\n"
)


def extract_triples(chunk: str, client, model: str):
    """Ask Mistral for triples from one chunk; tolerate minor JSON noise."""
    resp = client.chat.complete(
        model=model,
        messages=[{"role": "user", "content": EXTRACT_PROMPT + chunk}],
        temperature=0,
    )
    raw = resp.choices[0].message.content.strip()
    # Strip code fences if the model wrapped the JSON.
    raw = re.sub(r"^```(?:json)?|```$", "", raw, flags=re.MULTILINE).strip()
    try:
        items = json.loads(raw)
    except json.JSONDecodeError:
        return []
    out = []
    for it in items if isinstance(items, list) else []:
        s, p, o = it.get("subject"), it.get("predicate"), it.get("object")
        if s and p and o is not None:
            out.append((str(s), str(p), str(o), bool(it.get("object_is_entity", True))))
    return out


def slug(name: str) -> str:
    """Entity local name: keep alphanumerics, collapse the rest to underscores."""
    s = re.sub(r"[^0-9A-Za-z]+", "_", name.strip()).strip("_")
    return s or "_"


def camel(pred: str) -> str:
    """Predicate local name in camelCase (e.g. 'is author of' -> 'isAuthorOf')."""
    words = re.sub(r"[^0-9A-Za-z]+", " ", pred.strip().lower()).split()
    if not words:
        return "relatedTo"
    return words[0] + "".join(w.capitalize() for w in words[1:])


def esc(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")


def to_ntriples(triples) -> str:
    """Serialize extracted triples to N-Triples, adding rdfs:label per entity."""
    lines = []
    labels = {}  # iri -> label, so each entity gets exactly one label triple
    for s, p, o, o_is_entity in triples:
        s_iri = ENT + slug(s)
        labels[s_iri] = s
        p_iri = PROP + camel(p)
        if o_is_entity:
            o_iri = ENT + slug(o)
            labels[o_iri] = o
            lines.append(f"<{s_iri}> <{p_iri}> <{o_iri}> .")
        elif re.fullmatch(r"-?\d+", o.strip()):
            lines.append(f'<{s_iri}> <{p_iri}> "{o.strip()}"^^<{XSD_INT}> .')
        else:
            lines.append(f'<{s_iri}> <{p_iri}> "{esc(o)}" .')
    for iri, label in labels.items():
        lines.append(f'<{iri}> <{RDFS_LABEL}> "{esc(label)}" .')
    # Deduplicate while keeping order stable.
    seen, uniq = set(), []
    for line in lines:
        if line not in seen:
            seen.add(line)
            uniq.append(line)
    return "\n".join(uniq) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("title", nargs="?", default="The Hitchhiker's Guide to the Galaxy")
    ap.add_argument("--out", default="hitchhikers.nt")
    ap.add_argument("--chunk-size", type=int, default=3000)
    ap.add_argument("--model", default=os.environ.get("MISTRAL_MODEL", "mistral-small-latest"))
    args = ap.parse_args()

    api_key = os.environ.get("MISTRAL_API_KEY")
    if not api_key:
        sys.exit("MISTRAL_API_KEY is required for extraction (see .env.example).")
    try:
        from mistralai import Mistral
    except ImportError:
        sys.exit("pip install -r requirements.txt  (mistralai not found)")

    client = Mistral(api_key=api_key)
    print(f"Fetching '{args.title}' from Wikipedia ...", file=sys.stderr)
    text = fetch_extract(args.title)

    all_triples = []
    for i, chunk in enumerate(chunks(text, args.chunk_size), 1):
        print(f"  extracting chunk {i} ...", file=sys.stderr)
        all_triples.extend(extract_triples(chunk, client, args.model))

    nt = to_ntriples(all_triples)
    with open(args.out, "w", encoding="utf-8") as f:
        f.write(nt)
    print(f"Wrote {nt.count(chr(10))} triples to {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
