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

The graph itself is built from a Wikipedia article by `ingest_wikipedia.py`; a
pre-generated `hitchhikers.nt` (The Hitchhiker's Guide to the Galaxy) ships with
the example so this runs out of the box.

Run without an API key to see just the retrieved subgraph (retrieval-only mode).

Usage:
  # 1. serve the knowledge graph with Trillian (see README)
  ./target/release/server examples/graphrag/hitchhikers.nt 9090 &
  # 2. (optional) export your Mistral key for a generated answer
  export MISTRAL_API_KEY=...
  # 3. ask
  python3 examples/graphrag/graphrag.py "Who wrote the Hitchhiker's Guide and what is the answer to the Ultimate Question?"
"""

import os
import re
import sys
import json
import time
import resource
from datetime import datetime
import urllib.parse
import urllib.request

SPARQL_URL = os.environ.get("TRILLIAN_SPARQL", "http://localhost:9090/sparql")
RDFS_LABEL = "http://www.w3.org/2000/01/rdf-schema#label"
MODEL = os.environ.get("MISTRAL_MODEL", "mistral-small-latest")

# Metrics accumulators — populated by instrumented functions and printed at the end.
_METRICS: dict[str, object] = {}

def _memory_kb() -> int:
    """Resident set size in kB (macOS/Linux)."""
    return resource.getrusage(resource.RUSAGE_SELF).ru_maxrss // 1024

def _stamp() -> str:
    return datetime.now().time().isoformat(timespec="milliseconds")

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


def _uri(iri: str) -> dict:
    """A binding cell for a known IRI (mirrors SPARQL-results JSON shape)."""
    return {"type": "uri", "value": iri}


def neighbourhood(entity: str) -> list[tuple[dict, dict, dict]]:
    """Direct facts of `entity` (outgoing + incoming) plus one extra hop via a
    two-pattern join. Each term is a binding cell `{type, value}` so we can tell
    IRIs (resolve to labels) from literals (used verbatim)."""
    e = _uri(entity)
    triples: list[tuple[dict, dict, dict]] = []
    # outgoing: <e> ?p ?o
    for r in sparql(f"SELECT ?p ?o WHERE {{ <{entity}> ?p ?o }}"):
        triples.append((e, r["p"], r["o"]))
    # incoming: ?s ?p <e>
    for r in sparql(f"SELECT ?s ?p WHERE {{ ?s ?p <{entity}> }}"):
        triples.append((r["s"], r["p"], e))
    # one extra hop via a two-pattern BGP join: <e> ?p1 ?mid . ?mid ?p2 ?o2
    #   (this is what makes it "graph" RAG — structured multi-hop context).
    for r in sparql(
        f"SELECT ?mid ?p2 ?o2 WHERE {{ <{entity}> ?p1 ?mid . ?mid ?p2 ?o2 }} LIMIT 25"
    ):
        triples.append((r["mid"], r["p2"], r["o2"]))
    return triples


def term_str(cell: dict) -> str:
    """Render a term: IRIs become their label, literals are used as-is."""
    if cell["type"] == "uri":
        return label(cell["value"])
    return cell["value"]


def pred_label(p: str) -> str:
    """Turn a predicate IRI into words: writtenIn -> 'written in'."""
    name = p.rsplit("/", 1)[-1].rsplit("#", 1)[-1]
    if name == "type":
        return "is a"
    return re.sub(r"(?<!^)(?=[A-Z])", " ", name).lower()


def retrieve(question: str):
    """Return (fact_sentences, debug_entities)."""
    mem_before = _memory_kb()
    t0 = time.perf_counter()
    entities = match_entities(question)
    t1 = time.perf_counter()
    seen = set()
    facts = []
    for e in entities:
        for s, p, o in neighbourhood(e):
            if p["value"] == RDFS_LABEL:
                continue
            key = (s["value"], p["value"], o["value"])
            if key in seen:
                continue
            seen.add(key)
            facts.append(f"{term_str(s)} {pred_label(p['value'])} {term_str(o)}.")
    t2 = time.perf_counter()
    _METRICS["retrieval"] = {
        "entity_match_ms": round((t1 - t0) * 1000, 1),
        "neighbourhood_ms": round((t2 - t1) * 1000, 1),
        "total_retrieval_ms": round((t2 - t0) * 1000, 1),
        "memory_kb": _memory_kb() - mem_before,
    }
    return facts, [label(e) for e in entities]


def generate(question: str, facts: list[str]) -> str:
    """Ask Mistral to answer using only the retrieved facts."""
    api_key = os.environ.get("MISTRAL_API_KEY")
    if not api_key:
        return None
    try:
        from mistralai.client import Mistral
    except ImportError:
        print("(mistralai not installed — `pip install mistralai`; showing retrieval only)\n",
              file=sys.stderr)
        return None
    t0 = time.perf_counter()
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
    dt = time.perf_counter() - t0
    _METRICS["generation"] = {
        "llm_ms": round(dt * 1000, 1),
        "model": MODEL,
    }
    return resp.choices[0].message.content


def main():
    if len(sys.argv) < 2:
        print('usage: graphrag.py "your question"')
        sys.exit(1)
    question = " ".join(sys.argv[1:])

    t_start = time.perf_counter()

    facts, entities = retrieve(question)
    elapsed_ms = (time.perf_counter() - t_start) * 1000

    print(f"Q: {question}")
    print(f"  timestamp .. {_stamp()}")
    print(f"  process rss . {_memory_kb()} kB\n")
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
        print(f"  {answer}\n")

    # Metrics summary
    print("── Metrics ──────────────────────────────")
    print(f"  total wall .. {elapsed_ms:.0f} ms")
    print(f"  process rss . {_memory_kb()} kB")
    for phase, m in _METRICS.items():
        items = "  ".join(f"{k} {v}" for k, v in m.items())
        print(f"  {phase} ..... {items}")


if __name__ == "__main__":
    main()
