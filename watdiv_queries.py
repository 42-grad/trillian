#!/usr/bin/env python3
"""
watdiv_queries.py — generiert reale BGP-SPARQL-Queries aus einem .nt-Datensatz.

WatDivs eigener Query-Generator hat ein zickiges Output-Format; hier erzeugen
wir stattdessen BGP-Queries direkt aus den echten Daten – mit echten Konstanten
und WatDiv-typischen Shapes (Entity-Lookup, Property-Werte, 2-Hop-Pfad, Star,
inverse Lookup). Eine gebundene Konstante hält die Ergebnismengen klein und
bestimmt (wie bei den WatDiv-Templates).

Alle erzeugten Queries sind reines BGP-SELECT -> innerhalb unseres unterstützten
Feature-Sets, und gegen jeden konformen SPARQL-Endpoint identisch auswertbar.

Aufruf:  watdiv_queries.py <data.nt> <out_dir>
"""

import sys
from collections import defaultdict
from pathlib import Path


def parse_nt(path):
    """Liefert Liste von (s, p, o) als Term-Strings inkl. <>/\"\" (roh)."""
    triples = []
    with open(path, "r", encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            # robustes Splitten: erste zwei Tokens sind <iri>, Rest bis ' .' das Objekt
            if not line.endswith("."):
                continue
            body = line[:-1].strip()
            # Subjekt
            if not body.startswith("<"):
                continue
            se = body.find(">")
            s = body[: se + 1]
            rest = body[se + 1:].lstrip()
            if not rest.startswith("<"):
                continue
            pe = rest.find(">")
            p = rest[: pe + 1]
            o = rest[pe + 1:].strip()
            if "_:" in s or "_:" in o:  # Blank Nodes überspringen
                continue
            triples.append((s, p, o))
    return triples


def main():
    if len(sys.argv) != 3:
        print("usage: watdiv_queries.py <data.nt> <out_dir>")
        sys.exit(1)
    data, out_dir = sys.argv[1], Path(sys.argv[2])
    out_dir.mkdir(parents=True, exist_ok=True)

    triples = parse_nt(data)
    if not triples:
        print("Keine Tripel gelesen.")
        sys.exit(1)

    # Indizes
    by_pred = defaultdict(list)            # p -> [(s,o)]
    preds_of_subj = defaultdict(set)       # s -> {p}
    obj_subjects = defaultdict(set)        # o -> {s} (für inverse)
    for s, p, o in triples:
        by_pred[p].append((s, o))
        preds_of_subj[s].add(p)
        obj_subjects[o].add(s)

    # Prädikate nach Häufigkeit (mittelhäufige bevorzugen: nicht das größte,
    # damit Ergebnismengen bestimmt aber nicht riesig sind).
    preds = sorted(by_pred, key=lambda p: len(by_pred[p]), reverse=True)

    queries = {}

    # 1) Entity-Lookup: alle Properties eines konkreten Subjekts.
    #    Subjekt mit mittlerer Property-Anzahl wählen.
    rich_subj = max(preds_of_subj, key=lambda s: len(preds_of_subj[s]))
    queries["q01_entity"] = f"SELECT ?p ?o WHERE {{ {rich_subj} ?p ?o }}"

    # 2) Property-Werte: (S, P, ?o) für ein Subjekt + eines seiner Prädikate.
    p_of = sorted(preds_of_subj[rich_subj])[0]
    queries["q02_values"] = f"SELECT ?o WHERE {{ {rich_subj} {p_of} ?o }}"

    # 3) Inverse Lookup: ?s mit Prädikat P auf ein konkretes Objekt.
    #    Objekt mit mehreren eingehenden Kanten wählen.
    pop_obj = max(obj_subjects, key=lambda o: len(obj_subjects[o]))
    some_subj = next(iter(obj_subjects[pop_obj]))
    pred_in = next(p for s, p, o in triples if o == pop_obj and s == some_subj)
    queries["q03_inverse"] = f"SELECT ?s WHERE {{ ?s {pred_in} {pop_obj} }}"

    # 4) Star um ein gebundenes Objekt: ?s mit zwei Prädikaten, eines auf pop_obj.
    #    zweites Prädikat = beliebiges anderes Prädikat von some_subj.
    other_preds = sorted(preds_of_subj[some_subj] - {pred_in})
    if other_preds:
        p2 = other_preds[0]
        queries["q04_star"] = (
            f"SELECT ?s ?y WHERE {{ ?s {pred_in} {pop_obj} . ?s {p2} ?y }}"
        )

    # 5) 2-Hop-Pfad ab einem gebundenen Subjekt: <S> P1 ?mid . ?mid P2 ?o
    #    P1,P2 so wählen, dass die Objekte von P1 auch Subjekte von P2 sind.
    path = None
    for p1 in preds[:8]:
        mids = {o for _, o in by_pred[p1]}
        for p2 in preds[:8]:
            subs2 = {s for s, _ in by_pred[p2]}
            common = mids & subs2
            if common:
                # ein Startsubjekt von p1, dessen Objekt in common liegt
                for s1, o1 in by_pred[p1]:
                    if o1 in common:
                        path = (s1, p1, p2)
                        break
            if path:
                break
        if path:
            break
    if path:
        s1, p1, p2 = path
        queries["q05_path"] = (
            f"SELECT ?mid ?o WHERE {{ {s1} {p1} ?mid . ?mid {p2} ?o }}"
        )

    # 6) Predicate-Star (gebundenes Subjekt, zwei Prädikate) als zweite Star-Form.
    if len(preds_of_subj[rich_subj]) >= 2:
        ps = sorted(preds_of_subj[rich_subj])[:2]
        queries["q06_twostar"] = (
            f"SELECT ?a ?b WHERE {{ {rich_subj} {ps[0]} ?a . {rich_subj} {ps[1]} ?b }}"
        )

    # 7) ASK auf eine real existierende Kante.
    s0, p0, o0 = triples[0]
    queries["q07_ask"] = f"ASK {{ {s0} {p0} {o0} }}"

    for name, q in queries.items():
        (out_dir / f"{name}.rq").write_text(q + "\n")
    print(f"{len(queries)} Queries -> {out_dir}")
    for name in sorted(queries):
        print(f"  {name}: {queries[name][:90]}")


if __name__ == "__main__":
    main()
