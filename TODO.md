# Ausbau-TODO

Stand 2026-06-21. Echtdaten-Benchmark (WatDiv) ist abgeschlossen: Korrektheit
7/7 identisch zu Tentris, RAM-Parität (336 vs 319 B/Triple), ~21× kleiner auf
Disk, Ingest 3,9× schneller. Offene Punkte, priorisiert.

## 0. Profiling-Run (Speed + Memory) — ✅ ERLEDIGT 2026-06-21

Tooling: `server profile <file.nt> <query.rq> [runs]` (Phasen-Timing +
Memory-Report); `--features dhat` für reale Heap-Allokationen.
Gemessen auf WatDiv-Slice (1,09M Tripel).

**Speed-Befund (q04 star, 18995 Zeilen):** parse 0,015 ms · **eval 1,16 ms** ·
**serialize 25,27 ms**. → Die Engine ist schnell; **~95 % der Zeit großer
Queries geht in die SPARQL-JSON-Serialisierung** (`serde_json::Map`/`Value` pro
Zeile/Zelle). Das ist der gesamte „2×-Rückstand" zu Tentris.

**Memory-Befund (logisch 140 MB, dhat-Peak 300 MB / 711.922 Blöcke):**
- **Dictionary 66,7 MB + ~712k Allokationen** (jeder String **doppelt**:
  `id_to_str`-Value + `str_to_id`-Key) → größter Posten.
- **Stats-Pair-Count-Maps 40,7 MB / 2,28M Einträge** → #2.
- 3 Permutationen nur 29,9 MB (Index ist kompakt).
- Overhead logisch→real (~160 MB) = die winzigen Dict-Strings + Stats-Einträge.

## 1. Performance-Optimierungen (datengetrieben) — als nächstes

- [x] **(Speed) Streaming-SPARQL-JSON** — ✅ erledigt. Antwort wird direkt als
      String geschrieben (kein `Vec<Map<Value>>` mehr), Cache hält den fertigen
      Body, Content-Type jetzt `application/sparql-results+json`.
      Gemessen (q04, 18995 Zeilen): serialize **25,3 → 7,1 ms (3,6×)**, gesamt
      26,4 → 8,0 ms. Remote-Bestätigung gegen Tentris ausstehend.
- [ ] **(Memory, größter Hebel) Dictionary-String-Interning**: Strings in
      *einer* Arena (Buffer + Offsets) statt 712k Einzel-`String`s, und nur
      **einmal** halten (str_to_id → id, id_to_str → Offset). Erwartet
      Dict ~66 → ~33 MB + drastisch weniger Allokationen.
- [ ] **(Memory) Stats-Maps verschlanken**: 2,28M Pair-Count-Einträge (40 MB)
      für den Planner — on-demand aus dem Index ableiten oder kompakter halten.
- [ ] DELETE-Pfad: `pred_subjects`/`pred_objects` als `BTreeSet` statt sortiertem
      `Vec` (O(log n)- statt O(n)-Remove).
- [ ] WAL-Checkpointing / Snapshot-Rotation.

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
- [x] Content-Type `application/sparql-results+json` — ✅ (mit Streaming-JSON)

## 4. Benchmark-Hygiene

- [ ] **Tentris-`/update` liefert HTTP 404** (kein Update-Endpoint im gebauten
      Research-Build) → Update-Durchsatz gegen Tentris **nicht** vergleichbar.
      Frühere synthetische „Tentris ~1M Updates/s"-Zahlen waren 404-Artefakte
      (Durchsatz wurde auch bei Fehlerstatus berechnet). Update-Achse als
      „Rust-only (durabel)" kennzeichnen oder Tentris-Update-Pfad klären.
- [ ] Feste Perf-Suite mit großen-Ergebnis-Queries (q03/q04) als Regressionswächter.
