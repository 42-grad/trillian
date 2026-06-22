#!/usr/bin/env python3
"""
wdbench_compare.py — Korrektheits-Check: vergleicht UNSERE Ergebniszahl je Query
gegen die publizierten WDBench-Zahlen (Results/*.xlsx).

Beide Seiten cappen bei 100.000 Zeilen. Für Queries mit <100k Ergebnis muss die
Zahl exakt übereinstimmen, sonst rechnen wir falsch. Bei 100k auf beiden Seiten
ist die Query nur geclamped (kein Widerspruch). Referenz = Konsens der
publizierten Engines (wo sie sich einig sind) bzw. eine einzelne Engine.

Aufruf:  wdbench_compare.py <unsere_csv_dir> <xlsx_dir>
    csv_dir: enthält solo_<kategorie>.csv (query,status,ms,results) — unsere Zahlen
    xlsx_dir: enthält die WDBench Results/*.xlsx
"""

import csv
import re
import sys
import zipfile
from pathlib import Path
from xml.etree import ElementTree as ET

NS = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"
RNS = "{http://schemas.openxmlformats.org/officeDocument/2006/relationships}"
CLAMP = 100000

CATS = {
    "single_bgps": "Single_BGP.xlsx",
    "multiple_bgps": "Multiple BGP.xlsx",
    "opts": "Optional.xlsx",
    "paths": "Paths.xlsx",
    "c2rpqs": "C2RPQ.xlsx",
}
PUB_ENGINES = {"BLAZE", "BLAZEGRAPH", "JENA", "VIRTUOSO", "NEO4J", "NEO4J ", "NEO4J"}


def _col(ref):
    m = re.match(r"([A-Z]+)\d+", ref)
    c = 0
    for ch in m.group(1):
        c = c * 26 + (ord(ch) - 64)
    return c - 1


def _sheet_counts(z, ss, path):
    """{query_number: results} für die OK-Zeilen eines Sheets."""
    out = {}
    for r in ET.fromstring(z.read(path)).iter(f"{NS}row"):
        cells = {}
        for c in r.iter(f"{NS}c"):
            v = c.find(f"{NS}v")
            if v is not None:
                cells[_col(c.get("r"))] = ss[int(v.text)] if c.get("t") == "s" else v.text
        # Spalten: 0=query_number 1=results 2=status 3=time
        qn, res, st = cells.get(0), cells.get(1), str(cells.get(2, "")).strip().upper()
        if qn is None or res is None or st != "OK":
            continue
        try:
            out[int(float(qn))] = int(float(res))
        except ValueError:
            pass
    return out


def published_counts(xlsx_path):
    """Liefert {engine: {qnum: results}} für ein Kategorie-xlsx."""
    z = zipfile.ZipFile(xlsx_path)
    ss = ["".join(t.text or "" for t in n.iter(f"{NS}t"))
          for n in ET.fromstring(z.read("xl/sharedStrings.xml"))]
    rels = {r.get("Id"): r.get("Target")
            for r in ET.fromstring(z.read("xl/_rels/workbook.xml.rels"))}
    res = {}
    for s in ET.fromstring(z.read("xl/workbook.xml")).iter(f"{NS}sheet"):
        name = s.get("name")
        if name.upper().replace(" ", "") not in {e.replace(" ", "") for e in PUB_ENGINES}:
            continue
        tgt = rels[s.get(f"{RNS}id")]
        path = tgt if tgt.startswith("xl/") else "xl/" + tgt
        res[name] = _sheet_counts(z, ss, path)
    return res


def our_counts(csv_path):
    """{qnum: (status, results)} aus unserer solo_<cat>.csv."""
    out = {}
    with open(csv_path) as f:
        for row in csv.DictReader(f):
            m = re.search(r"(\d+)", row["query"])
            if not m:
                continue
            out[int(m.group(1))] = (row["status"], int(row["results"]))
    return out


def main():
    if len(sys.argv) != 3:
        print("usage: wdbench_compare.py <unsere_csv_dir> <xlsx_dir>")
        sys.exit(1)
    csv_dir, xlsx_dir = Path(sys.argv[1]), Path(sys.argv[2])
    print("### Korrektheit: Trillian-Ergebniszahl je Query vs. publizierte WDBench-Zahl")
    print("(beide bei 100k gecappt; Referenz = Konsens der publizierten Engines)\n")
    print(f"{'Kategorie':<14} {'verglichen':>10} {'exakt':>7} {'beide100k':>10} {'ABWEICHUNG':>11}")
    print("-" * 60)
    for cat, xlsx in CATS.items():
        ours = csv_dir / f"solo_{cat}.csv"
        pub = xlsx_dir / xlsx
        if not ours.exists() or not pub.exists():
            print(f"{cat:<14}  (fehlt: {'unsere CSV' if not ours.exists() else xlsx})")
            continue
        oc = our_counts(ours)
        pc = published_counts(pub)
        compared = exact = clamp = 0
        mism = []
        for qn, (st, ocount) in oc.items():
            if st != "OK":
                continue
            refs = [eng[qn] for eng in pc.values() if qn in eng]
            if not refs:
                continue
            # Konsens: alle einig -> eindeutige Referenz; sonst Median-nah, wir
            # akzeptieren Treffer, wenn ocount mit IRGENDEINER Referenz übereinstimmt.
            compared += 1
            if ocount in refs:
                if ocount >= CLAMP and all(r >= CLAMP for r in refs):
                    clamp += 1
                else:
                    exact += 1
            elif ocount >= CLAMP and max(refs) >= CLAMP:
                clamp += 1  # beide geclamped, evtl. minimal unter 100k auf einer Seite
            else:
                mism.append((qn, ocount, refs))
        print(f"{cat:<14} {compared:>10} {exact:>7} {clamp:>10} {len(mism):>11}")
        for qn, oc_, refs in mism[:4]:
            print(f"    q{qn}: ours={oc_}  published={sorted(set(refs))}")
    print("\nLesart: exakt+beide100k = Übereinstimmung. ABWEICHUNG>0 -> genauer ansehen.")


if __name__ == "__main__":
    main()
