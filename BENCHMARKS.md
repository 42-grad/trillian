# Benchmark-Messreihe

Chronologische Messreihe des Rust-Klons gegen die C++ Tentris-Forschungsversion,
um den Entwicklungs- und Verbesserungsprozess nachvollziehbar zu tracken.

- **Datensatz:** `synthetic_1m.nt` — 1.000.000 Triples, graph-förmig (gemeinsames
  `entity_*`-Vokabular, 1000 Dreiecke + 2000 Ketten eingepflanzt, seed-fix).
- **Harness:** `final_duel.py` v2 (Keep-Alive-Client, je Query Cold + 200 Warm,
  Median/p95; Update-Throughput; Memory via `/proc`). Remote auf Hetzner.
- **Ziel:** Tentris-Niveau bei Memory (≈281 B/Triple) bei Erhalt von
  Latenz-/Update-Vorsprung und Korrektheit.
- Roh-Logs unter `bench/`.

## Verlauf — Kernmetriken (Rust)

| Datum | Commit | Stufe | Peak-RSS | B/Triple | Ingest | INSERT/s | triangle med | chain med | Korrektheit |
| :-- | :-- | :-- | --: | --: | --: | --: | --: | --: | :-- |
| 2026-06-20 | `8852869` | Baseline (3 Perm. + Relationen) | 1053 MB | 1104 B | 1807 ms | 6,49M | 4,00 ms | 6,76 ms | alle Rows = Tentris |
| 2026-06-20 | `c3a8d63` | Etappe 1 (Relationen entfernt) | **906 MB** | **950 B** | **1406 ms** | **10,30M** | 4,32 ms | 7,17 ms | alle Rows = Tentris |

Referenz Tentris (Forschungsversion, gleicher Lauf): 268 MB RSS / 281 B/Triple
(metall-Disk-Store 513 MB); triangle ~11 ms, chain ~15 ms median.

## Korrektheit (jede Stufe identisch zu Tentris)

| Query | Rows (Rust = Tentris) |
| :-- | --: |
| chain | 3584 |
| triangle (WCOJ) | 3000 |
| star | 821 |
| distinct | 13207 |
| optional | 16002 |

## Stufen-Notizen

### Baseline — `8852869`
Drei BTreeMap-Permutationen (SPO/POS/OSP) + per-Prädikat Forward/Reverse-CSR-
Relationen für WCOJ. Inkrementelle Updates, volle SPARQL-Feature-Parität.
Erste valide Messung auf graph-förmigen Daten. **Memory ist die Schwäche**
(1104 B/Triple ≈ 3,9× Tentris) — dominiert vom Allokations-Overhead (Millionen
kleiner `Vec`-Blätter + BTreeMap-Knoten) und 5 Datenkopien.

### Etappe 1 — `c3a8d63` — Relationen eliminiert
Forward/Reverse-Relationen entfernt; WCOJ liest `objects_of`/`subjects_of` direkt
aus SPO/POS und hält nur noch schlanke, sortierte distinkte Subjekt-/Objekt-
Listen je Prädikat.
- **Memory −14 %** (1053 → 906 MB, 1104 → 950 B/Triple).
- **Ingest −22 %**, **INSERT +59 %** (10,3M/s) — keine Relation-BTreeMaps mehr
  beim Einfügen.
- Keine Latenz-Regression, Korrektheit unverändert.

## Offen (Roadmap, siehe BASELINE.md)

- **Etappe 2 (großer Memory-Hebel):** BTreeMap-Permutationen → flache CSR-Arenas
  (wenige große Allokationen) + Delta-Overlay für Updates. Ziel: Großteil der
  verbleibenden 950 B/Triple → Richtung 281 B/Triple.
- **Etappe 3:** Singleton-Kompression + Subtrie-Sharing (zahlt v. a. auf echten,
  strukturierten RDF-Daten — auf zufälligen Synthetikdaten wenig Effekt).
- OPTIONAL in die Engine ziehen (einzige Query-Niederlage: 38 vs. 30 ms).
