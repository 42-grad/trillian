# Ausbau-TODO

Stand 2026-06-21. Echtdaten-Benchmark (WatDiv) ist abgeschlossen: Korrektheit
7/7 identisch zu Tentris, RAM-Parität (336 vs 319 B/Triple), ~21× kleiner auf
Disk, Ingest 3,9× schneller. Offene Punkte, priorisiert.

## 0. Profiling-Run zuerst (Speed + Memory) — NÄCHSTER SCHRITT

Bevor optimiert wird: datengetrieben herausfinden, **wo** wir Performance liegen
lassen.

- [ ] **Speed-Profiling** der langsamen Queries (v. a. `q04 star`, 18995 Zeilen,
      97 vs. 50 ms; `q03 inverse`, 5862 Zeilen). Wo geht die Zeit hin?
      Kandidaten: SPARQL-Parse · Planung · Join/Leapfrog · Row-Materialisierung
      (`RowBlock`) · **JSON-Serialisierung** (`serde_json::Map` pro Zeile).
      - Tooling: `cargo flamegraph` (Linux `perf`) oder `samply`; alternativ
        Phasen-Timing im Server (parse/exec/serialize getrennt messen).
- [ ] **Memory-Profiling** bei Load + Query. Wo sitzt der RSS?
      Kandidaten: Index-Arenas (SPO/POS/OSP) · Dict-Strings (doppelt gehalten) ·
      Stats-Maps (Pair-Counts ~1 Eintrag/distinktes Paar) · Query-Zwischenergebnisse.
      - Tooling: `dhat-rs` (Heap-Profiler) oder `valgrind massif`.
- [ ] Ergebnis: Hotspot-Liste → priorisierte Optimierungen für Abschnitt 1.

## 1. Performance-Optimierungen (datengetrieben nach Profiling)

- [ ] **Große Ergebnismengen ~2× langsamer als Tentris** (q04/q03). Hypothese:
      JSON-Output (eine `Map`-Allokation pro Zeile) + Binär-Join-Materialisierung.
      → SPARQL-JSON streamend/direkt schreiben statt `Vec<Map<..>>` aufzubauen;
      ggf. Join für große Zwischenergebnisse verbessern. **Profiling bestätigt die Ursache.**
- [ ] Dictionary hält jeden String doppelt (`str_to_id`-Key + `id_to_str`-Value)
      → String-Interning / einfache Speicherung (Memory: ~Rest-Abstand zu Tentris).
- [ ] DELETE-Pfad: `pred_subjects`/`pred_objects` als `BTreeSet` statt sortiertem
      `Vec` (O(log n)- statt O(n)-Remove bei Einzeltransaktionen).
- [ ] WAL-Checkpointing / Snapshot-Rotation (WAL wächst sonst unbegrenzt).

## 2. SPARQL-Feature-Ausbau (ermöglicht echte Query-Suiten statt nur BGP)

- [ ] **FILTER** (häufigstes Feature in realen Queries) — Erststart.
- [ ] UNION
- [ ] ORDER BY (inkl. Zusammenspiel mit DISTINCT/LIMIT)
- [ ] Aggregation: GROUP BY, COUNT/SUM/MIN/MAX/AVG
- [ ] Mehrfache/verschachtelte OPTIONAL, BIND, Sub-SELECT
- [ ] Property Paths (optional)

## 3. Datenmodell / Kompatibilität

- [ ] Blank Nodes (Parser überspringt sie aktuell)
- [ ] Turtle-Input (.ttl), nicht nur N-Triples
- [ ] Content-Type `application/sparql-results+json` (aktuell `application/json`)

## 4. Benchmark-Hygiene

- [ ] **Tentris-`/update` liefert HTTP 404** (kein Update-Endpoint im gebauten
      Research-Build) → Update-Durchsatz gegen Tentris **nicht** vergleichbar.
      Frühere synthetische „Tentris ~1M Updates/s"-Zahlen waren 404-Artefakte
      (Durchsatz wurde auch bei Fehlerstatus berechnet). Update-Achse als
      „Rust-only (durabel)" kennzeichnen oder Tentris-Update-Pfad klären.
- [ ] Feste Perf-Suite mit großen-Ergebnis-Queries (q03/q04) als Regressionswächter.
