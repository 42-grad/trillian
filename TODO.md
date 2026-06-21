# Ausbau-TODO

Stand 2026-06-21. Echtdaten-Benchmark (WatDiv) ist abgeschlossen: Korrektheit
7/7 identisch zu Tentris, RAM-Parität (336 vs 319 B/Triple), ~21× kleiner auf
Disk, Ingest 3,9× schneller. Offene Punkte, priorisiert.

**WDBench (Wikidata, 1,26 Mrd. Tripel) — vorbereitet:** Harness steht
(`wdbench_queries.py`, `wdbench_probe.sh`, `wdbench_duel.sh`). Voraussetzungen
erledigt: Stats-Maps on-demand (§1) + Property Paths/C2RPQs (§2). Probe auf
echten WDBench-Daten validiert: alle 5 Query-Klassen führen aus (inkl.
paths/c2rpqs), logischer Footprint ~111 B/Triple → Projektion **~130 GB
logisch** @1,26 Mrd. (Obergrenze; Index davon mmap-pageable). Voller Duell-Lauf
braucht eine Big-RAM-Box (≥256 GB, da Tentris ~340 B/Triple ≈ 400 GB).

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
- [x] **(Memory) Dictionary-String-Interning** — ✅ erledigt (`string-interner`,
      eine Arena). Dict 66,7 → 36,0 MB; logisch 140 → 109 MB (105 B/Triple).
      dhat Heap-Peak 300,8 → 264,8 MB, **Allokations-Blöcke @Peak 711.922 → 212**.
      Remote-Bestätigung (RSS) ausstehend.
- [x] **(Memory) Stats-Maps verschlanken** — ✅ erledigt. Die 3 Pair-Count-Maps
      + 2 Degree-Maps (2,28M Einträge / 40,7 MB @1M Tripel) komplett entfernt;
      Kardinalitäten kommen on-demand aus dem Index (`CardEstimator`-Trait,
      `LayeredIndex::count_one`/`count_two`, O(1)/O(log n)). Stats-Maps jetzt
      **0 MB** (memory_report bestätigt); skaliert für WDBench (1,26 Mrd. Tripel,
      ~50 GB gespart). Plan-Qualität unverändert (Optimizer-Tests grün), 44/44.
- [ ] DELETE-Pfad: `pred_subjects`/`pred_objects` als `BTreeSet` statt sortiertem
      `Vec` (O(log n)- statt O(n)-Remove).
- [ ] WAL-Checkpointing / Snapshot-Rotation.

## 2. SPARQL-Feature-Ausbau (ermöglicht echte Query-Suiten statt nur BGP)

- [x] **FILTER** — ✅ erledigt. SPARQL-Ausdrucks-Evaluator (3-wertige Logik,
      EBV): Vergleiche (numerisch/String/IRI), `=`/`!=`/`sameTerm`, `&&`/`||`/`!`,
      `BOUND`, Arithmetik, `IN`, `IF`, Funktionen STR/LANG/DATATYPE/STRLEN/
      U-LCASE/CONTAINS/STRSTARTS/STRENDS/isIRI/isLiteral/isNumeric/isBlank.
      Wird im WHERE (vor Projektion) und in ASK/`/count` angewandt. 38 Tests grün.
      Offen: REGEX + Custom-Funktionen (derzeit → Ausdrucksfehler ⇒ Zeile fällt raus).
- [x] **UNION** — ✅ erledigt. Rekursiver WHERE-Evaluator (`eval_where`) wertet
      beide Zweige aus und richtet die Spalten über die Variablen-Vereinigung mit
      NULL aus (`union_rows`). Tests: gleiche Var + abweichende Var-Mengen.
- [x] **ORDER BY** — ✅ erledigt. `OrderKey`/`sort_rows` mit typbewusstem Vergleich
      (numerisch vor lexikalisch), `DESC()` invertiert, stabil; greift vor
      LIMIT/OFFSET und nach DISTINCT. Tests: asc IRI, DESC+LIMIT, numerisch, DISTINCT.
- [ ] Aggregation: GROUP BY, COUNT/SUM/MIN/MAX/AVG
- [ ] Mehrfache/verschachtelte OPTIONAL, BIND, Sub-SELECT
- [x] **Property Paths** — ✅ erledigt. `/` (^, |, *, +, ?, !{…}) als gerichtete
      Mengen-Propagation (`step_forward`/`step_backward`, transitive Hülle per
      BFS bis Fixpunkt). Ein gebundener Endpunkt ⇒ Closure nur von dort
      (effizient); beide Variablen ⇒ Aufzählung über Startknoten. Sequenz `p1/p2`
      kommt von spargebra als BGP mit Blank-Node-Zwischenknoten ⇒ Blank Nodes in
      Queries werden jetzt als nicht-distinguierte Variablen (`__bn_*`) behandelt.
      Schaltet WDBench **Property Paths + C2RPQs** frei. 8 Tests (+/*/?///^/|/
      both-var/C2RPQ-Join), 52/52 grün.

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
