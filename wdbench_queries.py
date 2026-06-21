#!/usr/bin/env python3
"""
wdbench_queries.py — wandelt die WDBench-Query-Logs in ausführbare .rq-Dateien.

WDBench liefert pro Kategorie eine Textdatei (`single_bgps.txt`,
`multiple_bgps.txt`, `opts.txt`, `paths.txt`, `c2rpqs.txt`), eine Query pro
Zeile im Format `<id>,<WHERE-Body>`. Der Body ist der Inhalt einer
WHERE-Klausel (Tripel-Muster / Property-Paths / OPTIONAL). Wir wrappen ihn zu
`SELECT * WHERE { <body> }` und schreiben je Query eine `.rq`-Datei in einen
Kategorie-Unterordner — direkt vergleichbar gegen jeden SPARQL-Endpoint.

Aufruf:  wdbench_queries.py <src_dir> <out_dir> [max_per_category]
    src_dir  enthält die fünf *.txt aus WDBench/Queries
    out_dir  Zielordner (ein Unterordner je Kategorie)
    max_per_category  optional: nur die ersten N Queries je Kategorie
"""

import sys
from pathlib import Path

CATEGORIES = {
    "single_bgps": "single_bgps.txt",
    "multiple_bgps": "multiple_bgps.txt",
    "opts": "opts.txt",
    "paths": "paths.txt",
    "c2rpqs": "c2rpqs.txt",
}


def convert_line(line: str) -> str | None:
    """`<id>,<body>` -> vollständige SELECT-*-Query (oder None bei Leerzeile)."""
    line = line.rstrip("\n")
    if not line.strip():
        return None
    # nur am ERSTEN Komma trennen (der Body enthält keine führende Zahl mehr).
    _id, _, body = line.partition(",")
    body = body.strip()
    if not body:
        return None
    # Body endet teils mit ". " — egal, SPARQL toleriert beides.
    return f"SELECT * WHERE {{ {body} }}"


def main():
    if len(sys.argv) < 3:
        print("usage: wdbench_queries.py <src_dir> <out_dir> [max_per_category]")
        sys.exit(1)
    src = Path(sys.argv[1])
    out = Path(sys.argv[2])
    cap = int(sys.argv[3]) if len(sys.argv) > 3 else None

    total = 0
    for cat, fname in CATEGORIES.items():
        fpath = src / fname
        if not fpath.exists():
            print(f"  ! {fname} fehlt in {src} — übersprungen")
            continue
        cdir = out / cat
        cdir.mkdir(parents=True, exist_ok=True)
        n = 0
        for i, line in enumerate(fpath.read_text(encoding="utf-8").splitlines(), 1):
            q = convert_line(line)
            if q is None:
                continue
            (cdir / f"q{i:05d}.rq").write_text(q + "\n")
            n += 1
            if cap and n >= cap:
                break
        total += n
        print(f"  {cat:<14} {n} Queries -> {cdir}")
    print(f"Gesamt: {total} Queries -> {out}")


if __name__ == "__main__":
    main()
