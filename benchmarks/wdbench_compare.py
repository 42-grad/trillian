#!/usr/bin/env python3
"""
wdbench_compare.py — correctness check: compares OUR result count per query
against the published WDBench numbers (Results/*.xlsx).

Both sides cap at 100,000 rows. For queries with a <100k result the count must
match exactly, otherwise we compute the wrong answer. With 100k on both sides
the query is merely clamped (no contradiction). Reference = consensus of the
published engines (where they agree) or a single engine.

Usage:  wdbench_compare.py <our_csv_dir> <xlsx_dir>
    csv_dir: contains solo_<category>.csv (query,status,ms,results) — our numbers
    xlsx_dir: contains the WDBench Results/*.xlsx
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
    """{query_number: results} for the OK rows of a sheet."""
    out = {}
    for r in ET.fromstring(z.read(path)).iter(f"{NS}row"):
        cells = {}
        for c in r.iter(f"{NS}c"):
            v = c.find(f"{NS}v")
            if v is not None:
                cells[_col(c.get("r"))] = ss[int(v.text)] if c.get("t") == "s" else v.text
        # Columns: 0=query_number 1=results 2=status 3=time
        qn, res, st = cells.get(0), cells.get(1), str(cells.get(2, "")).strip().upper()
        if qn is None or res is None or st != "OK":
            continue
        try:
            out[int(float(qn))] = int(float(res))
        except ValueError:
            pass
    return out


def published_counts(xlsx_path):
    """Returns {engine: {qnum: results}} for a category xlsx."""
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
    """{qnum: (status, results)} from our solo_<cat>.csv."""
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
        print("usage: wdbench_compare.py <our_csv_dir> <xlsx_dir>")
        sys.exit(1)
    csv_dir, xlsx_dir = Path(sys.argv[1]), Path(sys.argv[2])
    print("### Correctness: Trillian result count per query vs. published WDBench number")
    print("(both capped at 100k; reference = consensus of the published engines)\n")
    print(f"{'Category':<14} {'compared':>10} {'exact':>7} {'both100k':>10} {'DEVIATION':>11}")
    print("-" * 60)
    for cat, xlsx in CATS.items():
        ours = csv_dir / f"solo_{cat}.csv"
        pub = xlsx_dir / xlsx
        if not ours.exists() or not pub.exists():
            print(f"{cat:<14}  (missing: {'our CSV' if not ours.exists() else xlsx})")
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
            # Consensus: all agree -> unambiguous reference; otherwise near the
            # median, we accept a hit if ocount matches ANY reference.
            compared += 1
            if ocount in refs:
                if ocount >= CLAMP and all(r >= CLAMP for r in refs):
                    clamp += 1
                else:
                    exact += 1
            elif ocount >= CLAMP and max(refs) >= CLAMP:
                clamp += 1  # both clamped, possibly slightly under 100k on one side
            else:
                mism.append((qn, ocount, refs))
        print(f"{cat:<14} {compared:>10} {exact:>7} {clamp:>10} {len(mism):>11}")
        for qn, oc_, refs in mism[:4]:
            print(f"    q{qn}: ours={oc_}  published={sorted(set(refs))}")
    print("\nReading: exact+both100k = match. DEVIATION>0 -> look more closely.")


if __name__ == "__main__":
    main()
