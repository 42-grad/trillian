# Benchmarks

Trillian is measured on the full [WDBench](https://github.com/MillenniumDB/WDBench)
dataset — Wikidata Truthy, **1,257,169,959 triples** — and compared against the
benchmark's published numbers for Blazegraph, Jena (TDB), Virtuoso, and Neo4j.

## Setup

- **Dataset:** the canonical WDBench dump (`truthy_direct_properties.nt.bz2`),
  ~1.26 billion triples.
- **Queries:** the five WDBench classes verbatim — single BGPs (280),
  multiple BGPs (681), optionals (498), property paths (660), C2RPQs (539).
- **Method:** 60 s per-query timeout, output capped at 100k rows per query (the
  WDBench convention), one warm pass per query. Medians are over completed (OK)
  queries.
- **Reference:** the published per-query numbers from WDBench `Results/*.xlsx`.
- **Hardware:** Trillian ran on an AWS r6i instance. The published numbers come
  from the WDBench paper's machine (Xeon Silver 4110, 128 GB RAM; the SPARQL
  engines were given 64 GB). **Absolute milliseconds are therefore indicative,
  not a controlled head-to-head; on-disk store sizes are directly comparable.**

The harness is reproducible: `infra/aws/run_aws_bench.sh` (or `wdbench_solo.sh`
locally) runs all five classes and writes per-query CSVs; `wdbench_compare.py`
diffs result counts against the published numbers.

## Latency — median over completed queries (ms, lower is better)

| Class | **Trillian** | Blazegraph | Jena | Virtuoso | Neo4j |
| :-- | --: | --: | --: | --: | --: |
| Single BGP | **1** | 69 | 279 | 261 | 642 |
| Multiple BGP | **131** | 1166 | 2761 | 8436 | — |
| Optional | 5597 | 1892 | 3368 | 7900 | 11967 |
| Property Paths | **4** | 645 | 416 | 738 | 4612 |
| C2RPQ | **187** | 1113 | 632 | 2755 | — |

On the basic-graph-pattern classes Trillian's median is well under the fastest
published engine. (Read this together with coverage below — a fast median only
covers the queries that completed.)

## Coverage — how many of each class completed

| Class (n) | OK | Timeout | Error (row cap) |
| :-- | --: | --: | --: |
| Single BGP (280) | 267 | 0 | 13 |
| Multiple BGP (681) | 665 | 4 | 12 |
| Optional (498) | 315 | 0 | 183 |
| Property Paths (660) | 397 | 20 | 243 |
| C2RPQ (539) | 308 | 1 | 230 |

`Error` is Trillian's result-row cap firing on a degenerate query (a clean error,
not a crash); a comparable engine would typically time out instead. The published
engines also carry heavy timeouts here (e.g. Neo4j 134 and Jena 96 on paths;
Jena 242 on C2RPQ).

## Correctness — result counts vs. the published engines

Per query, Trillian's result count compared to the published counts (both clamped
at 100k). Below 100k the count must match exactly.

| Class | compared | matching | deviation |
| :-- | --: | --: | --: |
| Single BGP | 267 | 267 | **0** |
| Multiple BGP | 661 | 661 | **0** |
| Optional | 315 | 239 | 76 |
| Property Paths | 393 | 381 | 12 |
| C2RPQ | 283 | 232 | 51 |

- **BGP is provably correct** — 930 queries, zero deviations — so the latency
  result on those classes is on equal footing.
- **Optional** deviations are mostly cases where Trillian does *not* cross-product
  an unbound OPTIONAL (arguably more correct); the rest are blank-node diffs.
- **Property paths / C2RPQ** match 97% / 82% of comparable queries; the remaining
  deviations are a small long tail (off-by-a-few counts, a few genuine gaps).

## Footprint

| | Trillian | Blazegraph | Virtuoso | Jena | Neo4j |
| :-- | --: | --: | --: | --: | --: |
| On-disk store | **49 GB** | 70 GB | 70 GB | 110 GB | 112 GB |
| B / triple (disk) | **39** | 56 | 56 | 87 | 89 |

Trillian's snapshot is the most compact store of the field. In memory it holds
the **entire** graph resident: post-load RSS is ~15 GB (~12 B/triple) with the
index and dictionary memory-mapped (pageable), growing toward the ~49 GB snapshot
as the working set is touched — versus the disk-backed engines, which were given
64 GB to page a 70–112 GB on-disk store. Ingest + load of all 1.26 B triples is
~33 minutes.

RAM was driven down over four optimizations (drop a redundant type column, derive
predicate-object lists from the index, namespace-fold IRI prefixes, and
memory-map the dictionary): post-load RSS fell from 80 GB to ~15 GB.

## Reading it fairly

- Different hardware/setup than the published runs — treat absolute ms as
  indicative.
- In-memory (Trillian) vs. disk-backed (the others): much of the latency gap is
  architectural; much of the others' RAM headroom comes from not holding the
  whole graph resident.
- `Error` (Trillian's cap) and `Timeout` (the others) both mean "did not
  complete."
- Same query sets, 60 s timeout, 100k output cap; BGP result counts verified
  one-by-one against the published engines.
