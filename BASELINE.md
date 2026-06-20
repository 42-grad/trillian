# Baseline-Performance & Roadmap

Stand: **2026-06-20**. Diese Datei hält die erste valide Benchmark-Baseline
gegen die Tentris-Forschungsversion fest und listet die offenen Punkte.

## Setup

- **Datensatz:** `synthetic_1m.nt` — 1.000.000 Triples, **graph-förmig**
  (gemeinsames `entity_*`-Vokabular für Subjekt & Objekt), 50.100 eindeutige
  Terme, mit 1.000 eingepflanzten Dreiecken und 2.000 Ketten (siehe
  `src/synthetic.rs`, seed-fix für Reproduzierbarkeit).
- **Vergleich:** Rust-Klon (`/sparql`-Endpoint, in-memory) vs. C++ Tentris
  Forschungsversion (`tentris_server`, metall/mmap, persistent).
- **Harness:** `final_duel.py` v2 — Keep-Alive-HTTP-Client, je Query 1 Cold-Call
  (Cache-Miss) + 200 Warm-Calls, Median/p95; Update-Throughput; Memory via
  `/proc/<pid>/status`. Lauf auf einem Hetzner-Cloud-Server (`./run_remote_duel.sh`).

## Ergebnis (valider Lauf 2026-06-20)

Alle Queries liefern **identische Zeilenzahlen** wie Tentris (Korrektheit
bestätigt, inkl. zyklischem WCOJ-Triangle):

| Query | Rust median | Rust p95 | Tentris median | Tentris p95 | Rows (R=T) | Sieger (median) |
| :-- | --: | --: | --: | --: | :--: | :-- |
| chain | 6,76 ms | 8,07 ms | 15,18 ms | 55,01 ms | 3584 | Rust 2,2× |
| triangle (WCOJ) | 4,00 ms | 5,16 ms | 10,70 ms | 50,70 ms | 3000 | Rust 2,7× |
| star | 1,55 ms | 1,81 ms | 3,37 ms | 6,26 ms | 821 | Rust 2,2× |
| distinct | 10,10 ms | 13,93 ms | 12,98 ms | 53,80 ms | 13207 | Rust 1,3× |
| optional | 40,77 ms | 73,50 ms | 30,05 ms | 51,70 ms | 16002 | **Tentris 1,4×** |

| Metrik | Rust | Tentris | Sieger |
| :-- | --: | --: | :-- |
| Ingest + Startup | 1,81 s | 6,55 s | Rust 3,6× ¹ |
| INSERT (triples/s) | 6.490.106 | 1.013.238 | Rust 6,4× ² |
| DELETE (triples/s) | 1.962.666 | 681.534 | Rust 2,9× ² |
| **Peak-RSS** | **1053 MB** | 268 MB | **Tentris 3,9×** |
| RSS / VmRSS | 1010 MB | 268 MB | Tentris |
| Disk-Store | 98,6 MB (.nt-Quelle) | 512,9 MB (metall) | — |
| **Bytes / Triple (RSS)** | **1104 B** | 281 B | **Tentris 3,9×** |

¹ Tentris baut einen persistenten Disk-Index, wir in-RAM — kein reiner Apfel-Apfel-Vergleich.
² Rust-Updates sind in-RAM/nicht-durabel; Tentris persistiert. „Schnell-flüchtig" vs. „langsamer-dauerhaft".

## Interpretation

- **Korrektheit:** stärkstes Resultat — exakte Übereinstimmung auf allen 5
  Query-Formen inkl. WCOJ.
- **Latenz/Updates:** wir gewinnen auf diesem Workload — *mit Sternchen*: ein
  1-Mio-Synthetikgraph passt bequem in den RAM und spielt einer schlanken
  In-Memory-Struktur in die Hände. Tentris zielt auf große, persistente, reale
  RDF-Daten, wo Speichereffizienz und WCOJ-Asymptotik zählen. Der 2,7×-Triangle-
  Sieg ist real für dieses Setup, aber **kein genereller WCOJ-Überlegenheitsbeweis**.
- **Tails:** wir sind konstanter (Tentris p95 oft 5–10× Median, vermutlich
  mmap-Paging) — **außer bei OPTIONAL**.
- **OPTIONAL:** unsere einzige Query-Niederlage und algorithmische Schwäche —
  materialisierender Nested-Loop-Left-Join in der SPARQL-Schicht (pro linker
  Zeile `engine.execute` mit substituiertem Muster).
- **Memory:** klare, strukturelle Niederlage. 1104 B für ein 12-Byte-Triple
  ≈ 92× Overhead: 3 volle Permutationen (SPO/POS/OSP) + Forward/Reverse-
  Relationen pro Prädikat + BTreeMap-Knoten-Overhead. Der inkrementelle Umbau
  (BTreeMap statt flachem CSR) hat den Speicher bewusst gegen Update-Fähigkeit
  eingetauscht.
- **DELETE 3× langsamer als INSERT:** vermutlich `all_subjects/all_objects.remove()`
  (O(n)-Shift im 50k-Vec), wenn ein Subjekt seine letzte Kante verliert.

## Was bereits steht (Feature-Parität mit Tentris-Research)

- HTTP-Endpoints `/sparql`, `/stream`, `/count`, `/update`.
- SPARQL `SELECT`, `SELECT DISTINCT`, `ASK`, BGP, `OPTIONAL`, `LIMIT`/`OFFSET`
  (semantisch korrekte Reihenfolge: Projektion → DISTINCT → OFFSET/LIMIT).
- `INSERT DATA` / `DELETE DATA` mit RW-Lock und Cache-Invalidierung.
- **Inkrementelle** Index-Updates (kein Rebuild pro Triple).
- Volles RDF-Term-Modell (IRI, Literal mit Datentyp/Sprach-Tag), eigener
  N-Triples-Parser mit Escapes.
- LRU-Query-Cache; verlustfreie N-Triples-Persistenz (`TENTRIS_PERSIST=1`).

## Roadmap — was noch zu tun ist

Priorisiert nach Hebel; Zielmetriken jeweils aus der Baseline ableitbar.

### 1. Hypertrie-Umbau (größter Hebel) — adressiert Memory
Eine **einzige redundanzeliminierende Hypertrie mit Subtrie-Hashing**
(„Hashing the Hypertrie", ISWC 2022) statt 3 Permutationen + Per-Prädikat-
Relationen. Zieht den Join-Executor mit.
**Zielmetrik:** 1104 B/Triple → Richtung Tentris (≈281 B/Triple). Invasiv,
eigener Umbau — vorher/nachher gegen diese Baseline messen.

### 2. OPTIONAL in die Engine ziehen — adressiert einzige Query-Niederlage
Left-Join in die Tensor-/WCOJ-Evaluation integrieren statt SPARQL-Schicht-
Nested-Loop. **Zielmetrik:** optional 40,8 ms → unter Tentris (30 ms),
p95 73 ms → < 52 ms.

### 3. Uniformes Einsum-WCOJ
WCOJ über **beliebige** Muster (inkl. ungebundener Prädikate) statt Spezialfall
(gebundenes Prädikat + 2 Variablen) + Binär-Fallback.

### 4. mmap/persistente Indizes
Statt N-Triples-Dump: echter persistenter, memory-mapped Store → O(1)-Restart,
Write-Through ohne O(n)-Dump, geringerer RAM-Druck.

### Kleinere Punkte
- DELETE-Pfad: `all_subjects/all_objects` als Set/BTreeSet statt sortiertem Vec
  (O(log n)-Remove statt O(n)).
- SPARQL: `UNION`, `FILTER`, `BIND`, Sub-SELECT, Aggregation (`GROUP BY`/`COUNT`).
- Turtle-Input (.ttl), Blank Nodes.
- Content-Type `application/sparql-results+json` (aktuell `application/json`).
- README-Architektur aktualisieren („flat CSR" → BTreeMap-basiert seit inkrementellem Umbau).

## Reproduktion

```bash
# Lokal (nur Rust-Seite): Daten erzeugen + interner Bench
cargo run --release --bin tentris_clone

# Server starten
cargo build --release --bin server
./target/release/server synthetic_1m.nt 9081

# Volles Remote-Duell gegen C++ Tentris (Hetzner, braucht HCLOUD_TOKEN)
export HCLOUD_TOKEN=...
./run_remote_duel.sh        # Ergebnis -> duel_output.log
```

Tests: `cargo test` (27 Tests, alle grün).
