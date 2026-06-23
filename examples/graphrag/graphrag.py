#!/usr/bin/env python3
"""
graphrag.py — a minimal GraphRAG demo on top of Trillian + Mistral AI.

GraphRAG = Retrieval-Augmented Generation where the retrieval step walks a
knowledge *graph* instead of doing pure vector similarity. The benefit: the
context handed to the LLM is structured, multi-hop, and explainable — you can
see exactly which facts grounded the answer.

Pipeline:
  1. Match entities in the graph whose label matches the question's words
     (SPARQL FILTER/CONTAINS).
  2. Expand each entity's neighbourhood — its direct facts plus one extra hop
     via a two-pattern SPARQL join — to collect a relevant subgraph.
  3. Render those triples as readable sentences using rdfs:label.
  4. Ask Mistral to answer using ONLY those facts.

Run without an API key to see just the retrieved subgraph (retrieval-only mode).

Usage:
  # 1. build + serve the sample graph with Trillian (see README)
  ./target/release/server build examples/graphrag/knowledge.nt /tmp/kg.bin
  ./target/release/server load /tmp/kg.bin 9090 &
  # 2. (optional) export your Mistral key
  export MISTRAL_API_KEY=...
  # 3. ask
  python3 examples/graphrag/graphrag.py "What can I build with Trillian and Mistral AI?"
"""

import os
import re
import sys
import json
import urllib.parse
import urllib.request

SPARQL_URL = os.environ.get("TRILLIAN_SPARQL", "http://localhost:9090/sparql")
RDFS_LABEL = "http://www.w3.org/2000/01/rdf-schema#label"
MODEL = os.environ.get("MISTRAL_MODEL", "mistral-small-latest")

# Words too generic to be useful as entity-label matches.
STOPWORDS = {
    "what", "which", "who", "how", "is", "are", "the", "a", "an", "and", "or",
    "with", "for", "to", "of", "can", "do", "does", "i", "you", "in", "on",
    "tell", "me", "about", "use", "using", "build",
}


def sparql(query: str):
    """Run a SPARQL query against Trillian, return the bindings list."""
    url = SPARQL_URL + "?" + urllib.parse.urlencode({"query": query})
    req = urllib.request.Request(url, headers={"Accept": "application/sparql-results+json"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        data = json.loads(resp.read())
    return data.get("results", {}).get("bindings", [])


def label(iri: str) -> str:
    """Human-readable label for an IRI (rdfs:label, else the local name)."""
    rows = sparql(f"SELECT ?l WHERE {{ <{iri}> <{RDFS_LABEL}> ?l }} LIMIT 1")
    if rows:
        return rows[0]["l"]["value"]
    return iri.rsplit("/", 1)[-1]


def match_entities(question: str) -> list[str]:
    """Entities whose label contains a (non-stopword) token from the question."""
    tokens = [t for t in re.findall(r"[a-zA-Z]+", question.lower()) if t not in STOPWORDS and len(t) > 2]
    found: dict[str, None] = {}
    for tok in tokens:
        rows = sparql(
            f'SELECT ?e WHERE {{ ?e <{RDFS_LABEL}> ?l '
            f'FILTER(CONTAINS(LCASE(?l), "{tok}")) }}'
        )
        for r in rows:
            found[r["e"]["value"]] = None
    return list(found)


def neighbourhood(entity: str) -> list[tuple[str, str, str]]:
    """Direct facts of `entity` (outgoing + incoming) plus one extra hop via a
    property path, as (subject, predicate, object) IRI/literal triples."""
    triples: list[tuple[str, str, str]] = []
    # outgoing: <e> ?p ?o
    for r in sparql(f"SELECT ?p ?o WHERE {{ <{entity}> ?p ?o }}"):
        triples.append((entity, r["p"]["value"], r["o"]["value"]))
    # incoming: ?s ?p <e>
    for r in sparql(f"SELECT ?s ?p WHERE {{ ?s ?p <{entity}> }}"):
        triples.append((r["s"]["value"], r["p"]["value"], entity))
    # one extra hop via a two-pattern BGP join: <e> ?p1 ?mid . ?mid ?p2 ?o2
    #   (this is what makes it "graph" RAG — structured multi-hop context).
    for r in sparql(
        f"SELECT ?mid ?p2 ?o2 WHERE {{ <{entity}> ?p1 ?mid . ?mid ?p2 ?o2 }} LIMIT 25"
    ):
        triples.append((r["mid"]["value"], r["p2"]["value"], r["o2"]["value"]))
    return triples


def pred_label(p: str) -> str:
    """Turn a predicate IRI into words: writtenIn -> 'written in'."""
    name = p.rsplit("/", 1)[-1].rsplit("#", 1)[-1]
    if name == "type":
        return "is a"
    return re.sub(r"(?<!^)(?=[A-Z])", " ", name).lower()


def retrieve(question: str):
    """Return (fact_sentences, debug_entities)."""
    entities = match_entities(question)
    seen = set()
    facts = []
    for e in entities:
        for s, p, o in neighbourhood(e):
            key = (s, p, o)
            if key in seen or p == RDFS_LABEL:
                continue
            seen.add(key)
            facts.append(f"{label(s)} {pred_label(p)} {label(o)}.")
    return facts, [label(e) for e in entities]


def generate(question: str, facts: list[str]) -> str:
    """Ask Mistral to answer using only the retrieved facts."""
    api_key = os.environ.get("MISTRAL_API_KEY")
    if not api_key:
        return None
    try:
        from mistralai import Mistral
    except ImportError:
        print("(mistralai not installed — `pip install mistralai`; showing retrieval only)\n",
              file=sys.stderr)
        return None
    context = "\n".join(f"- {f}" for f in facts)
    prompt = (
        "Answer the question using ONLY the facts below. If the facts are not "
        "sufficient, say so. Be concise.\n\n"
        f"Facts:\n{context}\n\nQuestion: {question}"
    )
    client = Mistral(api_key=api_key)
    resp = client.chat.complete(
        model=MODEL,
        messages=[{"role": "user", "content": prompt}],
    )
    return resp.choices[0].message.content


def main():
    if len(sys.argv) < 2:
        print('usage: graphrag.py "your question"')
        sys.exit(1)
    question = " ".join(sys.argv[1:])

    facts, entities = retrieve(question)
    print(f"Q: {question}\n")
    print(f"Matched entities: {', '.join(entities) or '(none)'}")
    print("Retrieved subgraph:")
    for f in facts:
        print(f"  • {f}")
    print()

    if not facts:
        print("No relevant facts found in the graph.")
        return

    answer = generate(question, facts)
    if answer is None:
        print("(set MISTRAL_API_KEY to get a generated answer; retrieval shown above)")
    else:
        print("Answer (grounded in the subgraph above):")
        print(answer)


if __name__ == "__main__":
    main()
