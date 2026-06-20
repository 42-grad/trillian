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
| 2026-06-20 | `facb611` | Etappe 2 (flache CSR + Delta) | 676 MB | 709 B | 1005 ms | 6,99M | 4,42 ms | 6,69 ms | alle Rows = Tentris |
| 2026-06-20 | `da5cfad` | Quick Win (streaming ingest) | **338 MB** | **354 B** | **804 ms** | 9,43M | 4,26 ms | 7,12 ms | alle Rows = Tentris |

Referenz Tentris (Forschungsversion, gleicher Lauf): ~268 MB **RSS** / ~281 B/Triple
— aber das ist nur der resident Teil eines **513 MB großen metall-Disk-Stores**.
Unser Index liegt komplett im RAM (338 MB). Auf „Gesamtindex-Größe" sind wir
damit **kleiner** als Tentris (338 MB RAM vs. 513 MB Disk); auf „resident RAM
unter dieser Last" liegt Tentris 1,26× vorn. Memory-Lücke faktisch geschlossen.

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

### Quick Win — `da5cfad` — streaming ingest
`ingest_ntriples_file` parst zeilenweise und mappt sofort ins Dictionary; der
`ParsedTriple`-Puffer (~3M Strings) wird nie materialisiert.
- **Memory −50 %** (676 → 338 MB, 709 → 354 B/Triple). Abstand zu Tentris:
  2,5× → **1,26×** (bzw. kleiner als Tentris' 513-MB-Disk-Store).
- Ingest 1005 → 804 ms; Updates noch schneller (INSERT 9,43M/s, DELETE 15,2M/s).
- Latenz/Korrektheit unverändert.

### mmap-Persistenz — `6aa59d9` — fairer Vergleich (beide disk-backed)
Bis hierhin war der Vergleich Äpfel-vs-Birnen: Tentris persistent/mmap, wir
rein In-RAM. Jetzt hat der Rust-Klon einen **Loader/Server-Split wie Tentris**:
`server build` baut + persistiert einen Binär-Snapshot, `server load` mappt ihn
zero-copy. Das Duell misst beide gleich (Loader + mmap-Start).

Fairer Lauf (1M graph dataset):
- **Ingest+Startup:** Rust 1220 ms (Loader 831 + mmap-Start 148) vs. Tentris
  6476 ms → **Rust 5,3×** — jetzt legitim (beide bauen + persistieren + mmappen).
- **Memory:** Rust **310 B/Triple** (296 MB RSS, mmap-backed) vs. Tentris 281 B
  (268 MB) → **1,10×**, faktisch Gleichstand. Disk: unser Snapshot **35 MB** vs.
  Tentris' metall-Store **513 MB** → **~15× kleiner**.
- Latenz: chain 2,2×, triangle 2,6×, star 3,0×, distinct Gleichstand, optional
  Tentris 1,4×. Updates INSERT 7,3× / DELETE 14× — **aber mit Asterisk:** unsere
  Updates landen im RAM-Delta (nicht zurück in den Snapshot persistiert),
  Tentris-Updates sind durabel. Das ist der **letzte** verbleibende
  Äpfel-vs-Birnen-Punkt.
- Korrektheit über alle Queries identisch.

### Executor-Umbau (RowBlock) — `6d15c15` — flache Zeilen-Materialisierung
`Vec<Vec<u32>>` (eine Allokation pro Zeile) → flache row-major `RowBlock` (ein
Puffer). Binär-Planer + WCOJ + Projektion/DISTINCT/LIMIT + OPTIONAL-Join
schreiben direkt in den Puffer.
- **optional p95: 69,6 → 40,2 ms (−42 %)**, median 37,5 → 35,1 ms.
- chain 7,1 → 6,7 ms, triangle 4,3 → 4,0 ms; Memory stabil (332 MB).
- **optional bleibt aber Tentris 1,2×** (vorher 1,3×) — der Rest-Abstand liegt
  jetzt im Output-Pfad (JSON-Serialisierung von 16k Ergebniszeilen), nicht mehr
  in der Engine. Korrektheit über alle Queries identisch.

### OPTIONAL Hash-Join — `58052d7` — perf-neutral, Ursache identifiziert
OPTIONAL läuft jetzt als echter Hash-Left-Join (OPTIONAL-Muster einmal statt pro
linker Zeile). Korrektheit identisch, Code sauberer — aber auf diesem Benchmark
**perf-neutral** (optional ~37 ms, Tentris weiter ~1,3× vorn). Befund: der
Flaschenhals ist **nicht** der Join, sondern die executor-weite
`Vec<Vec<u32>>`-Zeilen-Materialisierung (eine Heap-Allokation pro Ergebniszeile;
bei optional ~100k kleine Allokationen über zwei 15k-Zwischenresultate + 16k
Ergebniszeilen). → adressiert vom Executor-Umbau (RowBlock, flache Bindings).

## Offen (Roadmap, siehe BASELINE.md)

Memory-Lücke ist faktisch geschlossen. Verbleibende Punkte:

- **Executor-Umbau (in Arbeit):** `Vec<Vec<u32>>` → flache row-major `RowBlock`
  (eine Allokation statt einer pro Zeile). Ziel: optional unter Tentris, generell
  niedrigere Query-Latenz + weniger Query-Peak-Memory.
- **Etappe 3 (optional):** Singleton-Kompression + Subtrie-Sharing — auf
  zufälligen Synthetikdaten wenig Effekt, lohnt erst auf echten RDF-Daten.
- Restliche ~73 B/Triple zu Tentris: Dict speichert Strings doppelt (Key +
  Value); String-Interning/Einfachspeicherung wäre der nächste Memory-Hebel.
