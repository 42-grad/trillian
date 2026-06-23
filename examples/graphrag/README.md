# Tutorial: GraphRAG with Trillian + Mistral AI

A minimal, runnable example of **GraphRAG** — Retrieval-Augmented Generation
where the retrieval step walks a knowledge **graph** (via SPARQL on Trillian)
instead of doing pure vector similarity. The model answers using a small,
structured, *explainable* subgraph.

```
question ──▶ Trillian (SPARQL retrieval) ──▶ subgraph of facts ──▶ Mistral ──▶ grounded answer
```

## Why a graph store for RAG?

Vector RAG retrieves text chunks by embedding similarity. Graph RAG retrieves a
**connected subgraph** by following relationships. That gives you:

- **Multi-hop context** — e.g. *Trillian → used for → GraphRAG → combines → LLM ← builds ← Mistral AI*. The link between two entities is found by traversing edges, not hoping it sits in one chunk.
- **Explainability** — you see exactly which triples grounded the answer.
- **Precision** — structured filters (types, relations) instead of fuzzy nearest-neighbours.

Trillian makes this cheap: entity lookups are sub-millisecond and joins are
worst-case-optimal, so retrieval stays fast even on large graphs.

## What's here

| File | Purpose |
| --- | --- |
| `knowledge.nt` | a tiny RDF knowledge graph (N-Triples) about Trillian, Mistral AI, and friends |
| `graphrag.py` | the demo: question → SPARQL retrieval → Mistral answer (stdlib only for retrieval) |
| `requirements.txt` | `mistralai` (only needed for the generation step) |

## Run it

### 1. Build Trillian and serve the knowledge graph

From the repository root:

```bash
cargo build --release --bin server
./target/release/server build examples/graphrag/knowledge.nt /tmp/kg.bin
./target/release/server load /tmp/kg.bin 9090 &
```

Trillian now serves SPARQL at `http://localhost:9090/sparql`.

### 2. Ask a question (retrieval only — no API key needed)

```bash
python3 examples/graphrag/graphrag.py "What can I build with Trillian and Mistral AI?"
```

You'll see the matched entities and the retrieved subgraph — the facts that
would be handed to the LLM. This works with just the Python standard library, so
you can explore retrieval before touching any API.

### 3. Add Mistral for the generated answer

```bash
pip install -r examples/graphrag/requirements.txt
export MISTRAL_API_KEY=...        # from https://console.mistral.ai/
python3 examples/graphrag/graphrag.py "What can I build with Trillian and Mistral AI?"
```

Now the retrieved subgraph is sent to Mistral with the instruction to answer
**using only those facts**, so the answer stays grounded in your graph.

## How it works (the 4 steps in `graphrag.py`)

1. **Entity matching** — split the question into words and find entities whose
   `rdfs:label` contains them:
   ```sparql
   SELECT ?e WHERE { ?e rdfs:label ?l FILTER(CONTAINS(LCASE(?l), "trillian")) }
   ```
2. **Neighbourhood expansion** — for each matched entity, gather its outgoing
   and incoming facts, plus one extra hop through a two-pattern join:
   ```sparql
   SELECT ?mid ?p2 ?o2 WHERE { <entity> ?p1 ?mid . ?mid ?p2 ?o2 }
   ```
   This is what makes it *graph* RAG: the context is a connected subgraph.
3. **Verbalize** — turn triples into readable sentences using `rdfs:label`
   (`Trillian writtenIn Rust` → "Trillian written in Rust").
4. **Generate** — prompt Mistral to answer using only those sentences.

## Make it yours

- Swap `knowledge.nt` for your own domain graph (any N-Triples file).
- Tune retrieval: add type filters, go deeper with property paths
  (`?x (ex:partOf)+ ?y`), or rank facts before sending them to the model.
- Point at a remote Trillian: `export TRILLIAN_SPARQL=http://host:9090/sparql`.
- Pick a model: `export MISTRAL_MODEL=mistral-large-latest`.
