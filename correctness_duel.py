#!/usr/bin/env python3
"""
correctness_duel.py

Strenger Korrektheits-Vergleich zweier SPARQL-Endpoints (Rust-Clone vs. Tentris):
nicht nur Zeilenzahlen, sondern die **vollständigen Binding-Mengen** werden als
Multimenge verglichen – nach Kanonisierung der RDF-Terme (Literal-Wert/Datatype,
plain == xsd:string, numerische Normalisierung).

Aufruf:
    correctness_duel.py <rust_url> <tentris_url> <query_dir> [--perf N]

`query_dir` enthält `*.rq`/`*.sparql`-Dateien (je eine SELECT/ASK-Query).
Pro Query: GET an beide Endpoints, Bindings normalisieren, vergleichen.
Klassifikation: IDENTICAL / ROWCOUNT_DIFF / BINDING_DIFF / RUST_ERR / TENTRIS_ERR.
Mit --perf wird zusätzlich die Warm-Median-Latenz beider Seiten gemessen.
"""

import http.client
import json
import statistics
import sys
import time
from collections import Counter
from pathlib import Path
from urllib.parse import urlencode, urlsplit

XSD = "http://www.w3.org/2001/XMLSchema#"
INT_TYPES = {
    XSD + t for t in (
        "integer", "int", "long", "short", "byte", "nonNegativeInteger",
        "positiveInteger", "nonPositiveInteger", "negativeInteger",
        "unsignedInt", "unsignedLong", "unsignedShort", "unsignedByte",
    )
}
FLOAT_TYPES = {XSD + "double", XSD + "float", XSD + "decimal"}


def http_get_sparql(url: str, query: str):
    """GET ?query=… mit SPARQL-JSON Accept. Liefert (status, parsed_json|None, raw)."""
    parts = urlsplit(url)
    conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=120)
    path = parts.path + "?" + urlencode({"query": query})
    conn.request("GET", path, headers={"Accept": "application/sparql-results+json"})
    resp = conn.getresponse()
    body = resp.read()
    conn.close()
    try:
        return resp.status, json.loads(body), body
    except json.JSONDecodeError:
        return resp.status, None, body


def norm_term(t: dict):
    """Kanonisiert einen SPARQL-JSON-Term zu einem vergleichbaren Tupel."""
    typ = t.get("type")
    val = t.get("value", "")
    if typ in ("uri", "bnode"):
        return (typ, val)
    # Literal
    lang = t.get("xml:lang")
    if lang:
        return ("lit", val, None, lang.lower())
    dt = t.get("datatype")
    # RDF 1.1: einfaches Literal == xsd:string
    if dt is None or dt == XSD + "string":
        dt = XSD + "string"
    v = val
    if dt in INT_TYPES:
        try:
            v = str(int(val))
        except ValueError:
            pass
    elif dt in FLOAT_TYPES:
        try:
            v = repr(float(val))
        except ValueError:
            pass
    elif dt == XSD + "boolean":
        v = "true" if val in ("true", "1") else "false"
    return ("lit", v, dt, None)


def canon_rows(result: dict):
    """Bindings -> Multimenge kanonischer Zeilen (über die Vereinigung der Vars)."""
    if result is None or "results" not in result:
        return None
    bindings = result.get("results", {}).get("bindings", [])
    vars_set = set(result.get("head", {}).get("vars", []))
    for b in bindings:
        vars_set.update(b.keys())
    vars_sorted = sorted(vars_set)
    rows = Counter()
    for b in bindings:
        row = tuple(
            (v, norm_term(b[v]) if v in b else None) for v in vars_sorted
        )
        rows[row] += 1
    return rows


def compare(rust: dict, tentris: dict):
    """Liefert (status, detail)."""
    if rust is not None and "error" in rust:
        return "RUST_ERR", rust.get("error", "")
    if tentris is not None and "error" in tentris:
        return "TENTRIS_ERR", tentris.get("error", "")
    # ASK
    if rust is not None and "boolean" in rust:
        rb = rust.get("boolean")
        tb = tentris.get("boolean") if tentris else None
        return ("IDENTICAL" if rb == tb else "BINDING_DIFF", f"ask rust={rb} tentris={tb}")
    rr = canon_rows(rust)
    tr = canon_rows(tentris)
    if rr is None or tr is None:
        return "PARSE_ERR", f"rust_parsed={rr is not None} tentris_parsed={tr is not None}"
    n_r = sum(rr.values())
    n_t = sum(tr.values())
    if rr == tr:
        return "IDENTICAL", f"{n_r} rows"
    only_rust = rr - tr
    only_tentris = tr - rr
    if n_r != n_t:
        kind = "ROWCOUNT_DIFF"
    else:
        kind = "BINDING_DIFF"
    sample_r = list(only_rust.elements())[:2]
    sample_t = list(only_tentris.elements())[:2]
    detail = (
        f"rust={n_r} tentris={n_t}; "
        f"only_rust={sample_r}; only_tentris={sample_t}"
    )
    return kind, detail


def warm_median_ms(url: str, query: str, runs: int) -> float:
    lat = []
    for _ in range(runs):
        t0 = time.perf_counter()
        http_get_sparql(url, query)
        lat.append((time.perf_counter() - t0) * 1000)
    return statistics.median(lat)


def main():
    if len(sys.argv) < 4:
        print("usage: correctness_duel.py <rust_url> <tentris_url> <query_dir> [--perf N]")
        sys.exit(1)
    rust_url, tentris_url, query_dir = sys.argv[1], sys.argv[2], sys.argv[3]
    perf_runs = 0
    if "--perf" in sys.argv:
        perf_runs = int(sys.argv[sys.argv.index("--perf") + 1])

    queries = sorted(Path(query_dir).glob("*.rq")) + sorted(Path(query_dir).glob("*.sparql"))
    if not queries:
        print(f"Keine Queries in {query_dir}")
        sys.exit(1)

    counts = Counter()
    print(f"{'Query':<24} {'Status':<14} Detail")
    print("-" * 100)
    for qf in queries:
        query = qf.read_text()
        _, rust, _ = http_get_sparql(rust_url, query)
        _, tentris, _ = http_get_sparql(tentris_url, query)
        status, detail = compare(rust, tentris)
        counts[status] += 1
        perf = ""
        if perf_runs and status == "IDENTICAL":
            rm = warm_median_ms(rust_url, query, perf_runs)
            tm = warm_median_ms(tentris_url, query, perf_runs)
            perf = f"  [rust {rm:.2f} ms | tentris {tm:.2f} ms]"
        print(f"{qf.name:<24} {status:<14} {detail}{perf}")

    print("-" * 100)
    total = sum(counts.values())
    ident = counts.get("IDENTICAL", 0)
    print(f"\nSUMMARY: {ident}/{total} IDENTICAL")
    for k, v in sorted(counts.items()):
        print(f"  {k}: {v}")
    # Exit-Code != 0, wenn nicht alles identisch (für CI/Skripte).
    sys.exit(0 if ident == total else 2)


if __name__ == "__main__":
    main()
