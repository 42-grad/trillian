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

import argparse
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


HTTP_TIMEOUT = 60  # s pro Query (WDBench-Stil); danach -> ERR, Lauf läuft weiter


def http_get_sparql(url: str, query: str):
    """GET ?query=… mit SPARQL-JSON Accept. Liefert (status, parsed_json|None, raw).

    Wirft NIE: Timeout/Connection-Fehler/Server-Tod werden als
    `(None, {"error": ...}, raw)` zurückgegeben, damit eine einzelne entartete
    Query den Gesamtlauf nicht abbricht (die Kategorie würde sonst sterben und
    nachfolgende Kategorien `Connection refused` bekommen)."""
    parts = urlsplit(url)
    conn = None
    try:
        conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=HTTP_TIMEOUT)
        path = parts.path + "?" + urlencode({"query": query})
        conn.request("GET", path, headers={"Accept": "application/sparql-results+json"})
        resp = conn.getresponse()
        body = resp.read()
        try:
            return resp.status, json.loads(body), body
        except json.JSONDecodeError:
            return resp.status, None, body
    except (TimeoutError, OSError, http.client.HTTPException) as e:
        kind = "timeout" if isinstance(e, TimeoutError) else type(e).__name__
        return None, {"error": f"{kind}: {e}"}, b""
    finally:
        if conn is not None:
            conn.close()


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


def canon_seq(result: dict):
    """Bindings -> GEORDNETE Liste kanonischer Zeilen (Reihenfolge erhalten).

    Für ORDER-BY-Queries: hier zählt die Sequenz, nicht nur die Multimenge."""
    if result is None or "results" not in result:
        return None
    bindings = result.get("results", {}).get("bindings", [])
    vars_set = set(result.get("head", {}).get("vars", []))
    for b in bindings:
        vars_set.update(b.keys())
    vars_sorted = sorted(vars_set)
    return [
        tuple((v, norm_term(b[v]) if v in b else None) for v in vars_sorted)
        for b in bindings
    ]


def snippet(raw: bytes) -> str:
    return (raw or b"").decode("utf-8", "replace").strip().replace("\n", " ")[:140]


def ask_value(parsed, raw: bytes):
    """Liest einen ASK-Wahrheitswert robust über verschiedene Serialisierungen:
    * SPARQL-Standard: `{"boolean": true|false}`
    * Tentris-Research: ASK als SELECT mit leerer Projektion
      (`{"results":{"bindings":[{}]}}` = true, `[]` = false)
    * nacktes `true`/`"true"` (text/boolean)
    Semantisch ist ASK genau dann true, wenn eine Lösung existiert."""
    if isinstance(parsed, dict):
        if "boolean" in parsed:
            return bool(parsed["boolean"])
        res = parsed.get("results")
        if isinstance(res, dict) and "bindings" in res:
            return len(res["bindings"]) > 0
    s = (raw or b"").decode("utf-8", "replace").strip().strip('"').lower()
    if s in ("true", "false"):
        return s == "true"
    return None


def compare(query: str, rust, tentris, rust_raw: bytes, tentris_raw: bytes):
    """Liefert (status, detail)."""
    if isinstance(rust, dict) and "error" in rust:
        return "RUST_ERR", rust.get("error", "")
    if isinstance(tentris, dict) and "error" in tentris:
        return "TENTRIS_ERR", tentris.get("error", "")
    # ASK (robust gegen abweichende Boolean-Repräsentationen)
    is_ask = query.lstrip().upper().startswith("ASK") or (
        isinstance(rust, dict) and "boolean" in rust
    )
    if is_ask:
        rb = ask_value(rust, rust_raw)
        tb = ask_value(tentris, tentris_raw)
        if rb is not None and rb == tb:
            return "IDENTICAL", f"ask={rb}"
        return "BINDING_DIFF", (
            f"ask rust={rb} tentris={tb}; tentris_raw={snippet(tentris_raw)}"
        )
    rr = canon_rows(rust)
    tr = canon_rows(tentris)
    if rr is None or tr is None:
        return "PARSE_ERR", (
            f"rust_parsed={rr is not None} tentris_parsed={tr is not None}; "
            f"rust_raw={snippet(rust_raw)}; tentris_raw={snippet(tentris_raw)}"
        )
    n_r = sum(rr.values())
    n_t = sum(tr.values())
    # ORDER BY: zusätzlich die SEQUENZ vergleichen (Multimenge allein ist blind
    # für die Sortierung). Tie-Mehrdeutigkeit umgehen unsere Queries durch
    # eindeutige Sortierschlüssel; gleiche Multimenge bei abweichender Sequenz
    # -> ORDER_DIFF. Abweichende Multimenge fällt in ROWCOUNT_/BINDING_DIFF.
    has_order_by = "order by" in " ".join(query.lower().split())
    if has_order_by and rr == tr:
        sr, st = canon_seq(rust), canon_seq(tentris)
        if sr == st:
            return "IDENTICAL", f"{n_r} rows (ordered)"
        first_diff = next(
            (i for i, (a, b) in enumerate(zip(sr, st)) if a != b), min(len(sr), len(st))
        )
        return "ORDER_DIFF", (
            f"{n_r} rows, gleiche Multimenge, Sequenz weicht ab @#{first_diff}: "
            f"rust={sr[first_diff:first_diff+1]} tentris={st[first_diff:first_diff+1]}"
        )
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


def latencies(url: str, query: str, runs: int):
    lat = []
    for _ in range(runs):
        t0 = time.perf_counter()
        http_get_sparql(url, query)
        lat.append((time.perf_counter() - t0) * 1000)
    lat.sort()
    median = statistics.median(lat)
    p95 = lat[max(0, int(len(lat) * 0.95) - 1)]
    return median, p95


def http_post(url: str, data: str, ctype: str):
    parts = urlsplit(url)
    conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=300)
    conn.request("POST", parts.path, body=data.encode("utf-8"), headers={"Content-Type": ctype})
    resp = conn.getresponse()
    body = resp.read()
    conn.close()
    return resp.status, body


def build_update(verb: str, k: int, base: int = 90_000_000) -> str:
    ns = "http://bench.local/"
    lines = [f"<{ns}e{base + i}> <{ns}p> <{ns}e{base + i + 1}> ." for i in range(k)]
    return f"{verb} {{ " + "\n".join(lines) + " }"


def read_proc_status(pid):
    """VmHWM (Peak) und VmRSS in KB aus /proc/<pid>/status (Linux)."""
    try:
        with open(f"/proc/{pid}/status") as f:
            text = f.read()
    except OSError:
        return None
    out = {}
    for line in text.splitlines():
        if line.startswith("VmHWM:"):
            out["peak_kb"] = int(line.split()[1])
        elif line.startswith("VmRSS:"):
            out["rss_kb"] = int(line.split()[1])
    return out or None


def dir_size(path: str) -> int:
    p = Path(path)
    if not p.exists():
        return 0
    if p.is_file():
        return p.stat().st_size
    return sum(f.stat().st_size for f in p.rglob("*") if f.is_file())


def fmt(v, unit, prec=2):
    return "n/a" if v is None else f"{v:.{prec}f} {unit}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("rust_url")
    ap.add_argument("tentris_url")
    ap.add_argument("query_dir")
    ap.add_argument("--perf", type=int, default=0, help="Warm-Durchläufe je Query (Median/p95)")
    ap.add_argument("--update", type=int, default=0, help="Triples für Update-Throughput")
    ap.add_argument("--triples", type=int, default=0, help="Datensatzgröße (für Bytes/Triple)")
    ap.add_argument("--rust-pid", type=int)
    ap.add_argument("--tentris-pid", type=int)
    ap.add_argument("--rust-disk")
    ap.add_argument("--tentris-disk")
    ap.add_argument("--ingest-rust", type=float, help="Rust Loader+Startup in ms")
    ap.add_argument("--ingest-tentris", type=float, help="Tentris Loader+Startup in ms")
    args = ap.parse_args()

    queries = sorted(Path(args.query_dir).glob("*.rq")) + sorted(Path(args.query_dir).glob("*.sparql"))
    if not queries:
        print(f"Keine Queries in {args.query_dir}")
        sys.exit(1)

    # --- Ingest / Startup ---
    if args.ingest_rust is not None or args.ingest_tentris is not None:
        print("### Ingest / Startup (echte Daten)")
        ir, it = args.ingest_rust, args.ingest_tentris
        winner = "—"
        if ir and it:
            winner = f"Rust {it/ir:.1f}x" if ir < it else f"Tentris {ir/it:.1f}x"
        print(f"  Rust {fmt(ir,'ms',0)} | Tentris {fmt(it,'ms',0)} | {winner}\n")

    # --- Korrektheit + Latenz ---
    print("### Korrektheit + Latenz")
    print(f"{'Query':<22} {'Status':<14} {'Rows':>7}  {'Rust med/p95':>16}  {'Tentris med/p95':>16}")
    print("-" * 92)
    counts = Counter()
    for qf in queries:
        query = qf.read_text()
        _, rust, rust_raw = http_get_sparql(args.rust_url, query)
        _, tentris, tentris_raw = http_get_sparql(args.tentris_url, query)
        status, detail = compare(query, rust, tentris, rust_raw, tentris_raw)
        counts[status] += 1
        rows = ""
        m = detail.split()[0] if detail and detail[0].isdigit() else ""
        rows = m
        lat = ""
        # Perf nur für saubere Vergleiche messen — sonst würden Timeouts (je
        # `runs` × HTTP_TIMEOUT) den Lauf massiv verzögern, ohne Aussagewert.
        if args.perf and status == "IDENTICAL":
            rmed, rp95 = latencies(args.rust_url, query, args.perf)
            tmed, tp95 = latencies(args.tentris_url, query, args.perf)
            lat = f"  {rmed:6.2f}/{rp95:6.2f} ms   {tmed:6.2f}/{tp95:6.2f} ms"
        extra = "" if status == "IDENTICAL" else f"  <- {detail}"
        print(f"{qf.name:<22} {status:<14} {rows:>7}{lat}{extra}")
    total = sum(counts.values())
    ident = counts.get("IDENTICAL", 0)
    print("-" * 92)
    print(f"Korrektheit: {ident}/{total} IDENTICAL  " + " ".join(f"{k}={v}" for k, v in sorted(counts.items())))

    # --- Update-Throughput (durabel) ---
    if args.update:
        print("\n### Update-Throughput (durabel)")
        k = args.update
        for label, url in (("Rust", args.rust_url), ("Tentris", args.tentris_url)):
            upd_url = url.replace("/sparql", "/update")
            t0 = time.perf_counter()
            st_i, body_i = http_post(upd_url, build_update("INSERT DATA", k), "application/sparql-update")
            ins = k / (time.perf_counter() - t0) if st_i in (200, 204) else None
            t0 = time.perf_counter()
            st_d, _ = http_post(upd_url, build_update("DELETE DATA", k), "application/sparql-update")
            dele = k / (time.perf_counter() - t0) if st_d in (200, 204) else None
            note = "" if ins is not None else f"  (INSERT HTTP {st_i}: {snippet(body_i)})"
            print(f"  {label:<8} INSERT {fmt(ins,'/s',0)} | DELETE {fmt(dele,'/s',0)}{note}")

    # --- Memory-Footprint ---
    if args.rust_pid or args.tentris_pid:
        print("\n### Memory-Footprint (echte Daten)")
        rm = read_proc_status(args.rust_pid) if args.rust_pid else None
        tm = read_proc_status(args.tentris_pid) if args.tentris_pid else None
        mb = lambda kb: None if kb is None else kb / 1024
        r_rss = rm.get("rss_kb") if rm else None
        t_rss = tm.get("rss_kb") if tm else None
        print(f"  Peak-RSS:  Rust {fmt(mb(rm.get('peak_kb') if rm else None),'MB')} | Tentris {fmt(mb(tm.get('peak_kb') if tm else None),'MB')}")
        print(f"  RSS:       Rust {fmt(mb(r_rss),'MB')} | Tentris {fmt(mb(t_rss),'MB')}")
        if args.rust_disk or args.tentris_disk:
            rd = dir_size(args.rust_disk) / 1024 / 1024 if args.rust_disk else None
            td = dir_size(args.tentris_disk) / 1024 / 1024 if args.tentris_disk else None
            print(f"  Disk:      Rust {fmt(rd,'MB')} (Snapshot) | Tentris {fmt(td,'MB')} (metall)")
        if args.triples and (r_rss or t_rss):
            bpt = lambda kb: None if kb is None else kb * 1024 / args.triples
            print(f"  Bytes/Triple (RSS): Rust {fmt(bpt(r_rss),'B',1)} | Tentris {fmt(bpt(t_rss),'B',1)}")

    sys.exit(0 if ident == total else 2)


if __name__ == "__main__":
    main()
