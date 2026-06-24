# Tutorial: GraphRAG over a Wikipedia page with Trillian + Mistral AI

A runnable, end-to-end **GraphRAG** example: turn a Wikipedia article into a
knowledge graph with an LLM, store it in Trillian, then answer questions by
walking that graph instead of doing vector similarity.

```
Wikipedia article
      │  ingest_wikipedia.py  (Mistral extracts triples)
      ▼
   knowledge graph (N-Triples)  ──load──▶  Trillian
                                              │
question ──▶ Trillian (SPARQL retrieval) ──▶ subgraph ──▶ Mistral ──▶ grounded answer
```

The shipped graph is built from **“The Hitchhiker's Guide to the Galaxy”** — a
fitting dataset for a store named Trillian.

## Why a graph store for RAG?

Vector RAG retrieves text chunks by embedding similarity. Graph RAG retrieves a
**connected subgraph** by following relationships, which gives you:

- **Multi-hop context** — e.g. *Arthur Dent → home planet → Earth → destroyed by → Vogons*. The link is found by traversing edges, not by hoping it sits in one chunk.
- **Explainability** — you see exactly which triples grounded the answer.
- **Precision** — structured filters (types, relations) instead of fuzzy nearest-neighbours.

Trillian makes this cheap: entity lookups are sub-millisecond and joins are
worst-case-optimal, so retrieval stays fast even on large graphs.

## What's here

| File | Purpose |
| --- | --- |
| `ingest_wikipedia.py` | **ingestion**: fetch a Wikipedia article → Mistral extracts triples → write N-Triples |
| `hitchhikers.nt` | a pre-generated knowledge graph (so the demo runs without an API key) |
| `graphrag.py` | **retrieval + answer**: question → SPARQL subgraph → Mistral answer |
| `requirements.txt` | `mistralai` (only needed for the LLM steps) |
| `.env.example` | the environment variables the scripts read |

## Run it

### 1. Build Trillian and serve the knowledge graph

From the repository root:

```bash
cargo build --release --bin server
./target/release/server examples/graphrag/hitchhikers.nt 9090 &
```

Trillian now serves SPARQL at `http://localhost:9090/sparql`.

### 2. Ask a question (retrieval only — no API key needed)

```bash
python3 examples/graphrag/graphrag.py "Who wrote the Hitchhiker's Guide and what is the answer to the Ultimate Question?"
```

You'll see the matched entities and the retrieved subgraph — the exact facts that
would be handed to the LLM. This uses only the Python standard library, so you
can explore retrieval before touching any API.

### 3. Add Mistral for the generated answer

```bash
pip install -r examples/graphrag/requirements.txt
export MISTRAL_API_KEY=...        # from https://console.mistral.ai/
python3 examples/graphrag/graphrag.py "How was Earth destroyed, and by whom?"
```

The retrieved subgraph is sent to Mistral with the instruction to answer **using
only those facts**, so the answer stays grounded in your graph.

## Ingest your own Wikipedia page

`hitchhikers.nt` was produced by the ingestion script — regenerate it, or build a
graph for any article:

```bash
export MISTRAL_API_KEY=...
python3 examples/graphrag/ingest_wikipedia.py "Douglas Adams" --out adams.nt
./target/release/server examples/graphrag/adams.nt 9090 &
python3 examples/graphrag/graphrag.py "When was Douglas Adams born?"
```

`ingest_wikipedia.py` fetches the article's plain text from the Wikipedia API,
chunks it, asks Mistral to extract `(subject, predicate, object)` triples, and
serializes them as N-Triples with `rdfs:label`s — exactly the shape `graphrag.py`
expects.

## How retrieval works (the 4 steps in `graphrag.py`)

1. **Entity matching** — find entities whose `rdfs:label` contains a question word:
   ```sparql
   SELECT ?e WHERE { ?e rdfs:label ?l FILTER(CONTAINS(LCASE(?l), "earth")) }
   ```
2. **Neighbourhood expansion** — gather each entity's outgoing and incoming facts
   plus one extra hop through a two-pattern join:
   ```sparql
   SELECT ?mid ?p2 ?o2 WHERE { <entity> ?p1 ?mid . ?mid ?p2 ?o2 }
   ```
   This is what makes it *graph* RAG: the context is a connected subgraph.
3. **Verbalize** — turn triples into sentences via `rdfs:label`
   (`Earth destroyedBy Vogons` → "Earth destroyed by Vogons").
4. **Generate** — prompt Mistral to answer using only those sentences.

## Make it yours

- Ingest any article with `ingest_wikipedia.py "<title>"`.
- Tune retrieval: add type filters, go deeper with property paths
  (`?x (ex:knows)+ ?y`), or rank facts before sending them to the model.
- Point at a remote Trillian: `export TRILLIAN_SPARQL=http://host:9090/sparql`.
- Pick a model: `export MISTRAL_MODEL=mistral-large-latest`.
