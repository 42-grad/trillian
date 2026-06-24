# WDBench — published reference numbers (comparison baseline)

Source: [WDBench](https://github.com/MillenniumDB/WDBench) `Results/*.xlsx` (Angles
et al., ISWC 2022). Full **1,257,169,959-triple** Wikidata Truthy graph,
**60-s timeout**, times in **milliseconds**, each query run once.
Source columns: `query_number, results, status, time` (status: OK/TIMEOUT/ERROR).

Query sets identical to `Queries/*.txt`: single_bgps **280**, multiple_bgps
**681**, opts **498**, paths **660**, c2rpqs **539**.

## Median (ms) of the OK queries + timeouts

| Category (n) | Blazegraph | Jena | Virtuoso | Neo4j |
| :-- | --: | --: | --: | --: |
| Single BGP (280) | **69** | 279 | 261 | 642 |
| Multiple BGP (681) | **1166** | 2761 | 8436 | — |
| Optional (498) | **1892** | 3368 | 7900 | 11967 |
| Paths (660) | 645 | **416** | 738 | 4612 |
| C2RPQ (539) | 1113 | **632** | 2755 | — |

Timeouts (of n), the harshest excerpt:
- Single BGP: Neo4j 47, Jena 23, Blaze 3, Virtuoso 1
- Multiple BGP: Blaze 47, Jena 46, Virtuoso 6
- Optional: Neo4j 146, Virtuoso 69, Jena 41, Blaze 28
- Paths: Neo4j 134, Jena 96, Blaze 87, Virtuoso 24 (+27 ERR)
- C2RPQ: **Jena 242, Blaze 178**, Virtuoso 37 (even mature engines fail here in droves)

## Comparison methodology for us (Trillian)

`wdbench_solo.sh` + `wdbench_bench.py` produce exactly this format for our
engine: the same query sets, 60-s timeout, median/avg/quartiles of the OK times +
TIMEOUT/ERROR counts. `ERROR` for us = row cap (cross product) or parse — on the
reference engines such queries would typically count as TIMEOUT.

Data source: the **canonical** Figshare dump `truthy_direct_properties.nt.bz2`
(article 19599589, 9.15 GB, md5 `b3ef85c9…`) → ~1.257 billion triples. **Not** the
truncated `latest_truthy_data_filtered.tar.bz2` (article 23994126, 3.59 GB), which
no tool decompresses beyond ~495M lines.

Caveats: different hardware, single-thread vs. our parallelism — absolute
milliseconds are therefore indicative, not a controlled head-to-head.
