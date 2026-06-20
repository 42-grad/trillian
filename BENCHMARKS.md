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
| 2026-06-20 | `c3a8d63` | Etappe 1 (Relationen entfernt) | 906 MB | 950 B | 1406 ms | 10,30M | 4,32 ms | 7,17 ms | alle Rows = Tentris |
| 2026-06-20 | `facb611` | Etappe 2 (flache CSR + Delta) | **676 MB** | **709 B** | **1005 ms** | 6,99M | 4,42 ms | 6,69 ms | alle Rows = Tentris |

Referenz Tentris (Forschungsversion, gleicher Lauf): ~266 MB RSS / ~279 B/Triple
(metall-Disk-Store 513 MB); triangle ~11–12 ms, chain ~15 ms median.

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

### Etappe 2 — `facb611` — flache CSR-Arenas + Delta-Overlay
BTreeMap-Permutationen (Millionen Klein-Allokationen) → kompakte flache CSR-Basis
(wenige große Vektoren) + kleines Delta (`ins`/`del`). `query_two` liefert
`Cow::Borrowed` ohne Delta-Treffer, sonst gemergt; Delta wird bei Bedarf in die
Basis gefaltet → Updates bleiben inkrementell.
- **Memory −25 %** ggü. Etappe 1 (906 → 676 MB, 950 → 709 B/Triple); −36 % ggü.
  Baseline. Abstand zu Tentris: 3,4× → **2,5×**.
- **Ingest −29 %** (1406 → 1005 ms).
- Updates weiter schnell (INSERT 6,99M/s, DELETE 10,55M/s; je 8,2× Tentris).
- Keine Latenz-Regression (distinct jetzt Gleichstand, optional nur noch 1,1×).

## Offen (Roadmap, siehe BASELINE.md)

- **Quick Win:** `server.rs` hält den geparsten Triple-Puffer (~3M Strings)
  über die gesamte Laufzeit – der Dict hat die Strings längst kopiert. Vor
  `serve()` droppen senkt den resident RSS spürbar (geschätzt ~150–200 MB).
- **Etappe 3:** Singleton-Kompression + Subtrie-Sharing (zahlt v. a. auf echten,
  strukturierten RDF-Daten — auf zufälligen Synthetikdaten wenig Effekt).
- OPTIONAL in die Engine ziehen (einzige Query-Niederlage: 34 vs. 32 ms).
