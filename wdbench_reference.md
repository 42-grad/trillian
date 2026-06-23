# WDBench — publizierte Referenzzahlen (Vergleichsbasis)

Quelle: [WDBench](https://github.com/MillenniumDB/WDBench) `Results/*.xlsx` (Angles
et al., ISWC 2022). Voller **1.257.169.959-Tripel** Wikidata-Truthy-Graph,
**60-s-Timeout**, Zeiten in **Millisekunden**, je Query einmal ausgeführt.
Spalten der Quelle: `query_number, results, status, time` (status: OK/TIMEOUT/ERROR).

Query-Sets identisch zu `Queries/*.txt`: single_bgps **280**, multiple_bgps
**681**, opts **498**, paths **660**, c2rpqs **539**.

## Median (ms) der OK-Queries + Timeouts

| Kategorie (n) | Blazegraph | Jena | Virtuoso | Neo4j |
| :-- | --: | --: | --: | --: |
| Single BGP (280) | **69** | 279 | 261 | 642 |
| Multiple BGP (681) | **1166** | 2761 | 8436 | — |
| Optional (498) | **1892** | 3368 | 7900 | 11967 |
| Paths (660) | 645 | **416** | 738 | 4612 |
| C2RPQ (539) | 1113 | **632** | 2755 | — |

Timeouts (von n), Auszug der härtesten:
- Single BGP: Neo4j 47, Jena 23, Blaze 3, Virtuoso 1
- Multiple BGP: Blaze 47, Jena 46, Virtuoso 6
- Optional: Neo4j 146, Virtuoso 69, Jena 41, Blaze 28
- Paths: Neo4j 134, Jena 96, Blaze 87, Virtuoso 24 (+27 ERR)
- C2RPQ: **Jena 242, Blaze 178**, Virtuoso 37 (selbst reife Engines scheitern hier massenhaft)

## Vergleichsmethodik für uns (Trillian)

`wdbench_solo.sh` + `wdbench_bench.py` erzeugen exakt dieses Format für unsere
Engine: dieselben Query-Sets, 60-s-Timeout, Median/AVG/Quartile der OK-Zeiten +
TIMEOUT/ERROR-Zähler. `ERROR` bei uns = Row-Cap (Cross-Product) oder Parse — bei
den Referenz-Engines würden solche Queries typischerweise als TIMEOUT zählen.

Datenquelle: der **kanonische** Figshare-Dump `truthy_direct_properties.nt.bz2`
(Artikel 19599589, 9,15 GB, md5 `b3ef85c9…`) → ~1,257 Mrd. Tripel. **Nicht** die
truncated `latest_truthy_data_filtered.tar.bz2` (Artikel 23994126, 3,59 GB), die
mit keinem Tool über ~495M Zeilen hinaus dekomprimiert.

Caveats: andere Hardware, single-thread vs. unsere Parallelität — absolute
Millisekunden sind daher indikativ, kein kontrollierter Head-to-Head.
