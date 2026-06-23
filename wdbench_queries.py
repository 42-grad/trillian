#!/usr/bin/env python3
"""
wdbench_queries.py — converts the WDBench query logs into executable .rq files.

WDBench provides one text file per category (`single_bgps.txt`,
`multiple_bgps.txt`, `opts.txt`, `paths.txt`, `c2rpqs.txt`), one query per line
in the format `<id>,<WHERE body>`. The body is the contents of a WHERE clause
(triple patterns / property paths / OPTIONAL). We wrap it into
`SELECT * WHERE { <body> }` and write one `.rq` file per query into a
category subfolder — directly comparable against any SPARQL endpoint.

Usage:  wdbench_queries.py <src_dir> <out_dir> [max_per_category]
    src_dir  contains the five *.txt from WDBench/Queries
    out_dir  target folder (one subfolder per category)
    max_per_category  optional: only the first N queries per category
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
    """`<id>,<body>` -> full SELECT-* query (or None on a blank line)."""
    line = line.rstrip("\n")
    if not line.strip():
        return None
    # split only at the FIRST comma (the body no longer has a leading number).
    _id, _, body = line.partition(",")
    body = body.strip()
    if not body:
        return None
    # The body sometimes ends with ". " — fine, SPARQL tolerates both.
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
            print(f"  ! {fname} missing in {src} — skipped")
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
        print(f"  {cat:<14} {n} queries -> {cdir}")
    print(f"Total: {total} queries -> {out}")


if __name__ == "__main__":
    main()
