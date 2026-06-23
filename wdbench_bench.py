#!/usr/bin/env python3
"""
wdbench_bench.py — single-engine WDBench benchmark in the published format.

Measures ONE SPARQL endpoint over a WDBench query directory and prints the
aggregates that the official `Results/*.xlsx` use (median/avg/quartiles of the
OK times in ms, plus TIMEOUT/ERROR counts at the 60-s timeout). No comparison
partner — we put our column directly next to the published
Blazegraph/Jena/Virtuoso/Neo4j numbers.

Status per query:
  OK      — bindings returned within the timeout
  TIMEOUT — wall time > --timeout (analogous to the reference's 60-s cutoff)
  ERROR   — engine error (e.g. row cap on a cross product, parse error)

Usage:
  wdbench_bench.py <endpoint_url> <query_dir> [--timeout 60] [--label trillian]
"""

import argparse
import http.client
import json
import statistics
import sys
import time
from pathlib import Path
from urllib.parse import urlencode, urlsplit


def run_query(url: str, query: str, timeout: float):
    """Returns (status, time_ms, n_results). status in OK/TIMEOUT/ERROR."""
    parts = urlsplit(url)
    conn = None
    t0 = time.perf_counter()
    try:
        conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=timeout)
        path = parts.path + "?" + urlencode({"query": query})
        conn.request("GET", path, headers={"Accept": "application/sparql-results+json"})
        resp = conn.getresponse()
        body = resp.read()
        dt = (time.perf_counter() - t0) * 1000.0
        try:
            parsed = json.loads(body)
        except json.JSONDecodeError:
            return "ERROR", dt, 0
        if isinstance(parsed, dict) and "error" in parsed:
            return "ERROR", dt, 0
        res = parsed.get("results", {}).get("bindings")
        if res is None:
            # ASK etc. -> count as OK with 0/1
            return "OK", dt, 1 if parsed.get("boolean") else 0
        return "OK", dt, len(res)
    except TimeoutError:
        return "TIMEOUT", timeout * 1000.0, 0
    except (OSError, http.client.HTTPException) as e:
        return "ERROR", (time.perf_counter() - t0) * 1000.0, str(e)
    finally:
        if conn is not None:
            conn.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("endpoint_url")
    ap.add_argument("query_dir")
    ap.add_argument("--timeout", type=float, default=60.0, help="timeout per query in s (reference: 60)")
    ap.add_argument("--out-limit", type=int, default=100000,
                    help="output cap per query like WDBench (0 = off). Appended as a LIMIT.")
    ap.add_argument("--label", default="trillian")
    ap.add_argument("--csv", help="optional: path for a per-query CSV (query,status,ms,results)")
    args = ap.parse_args()

    queries = sorted(Path(args.query_dir).glob("*.rq")) + sorted(Path(args.query_dir).glob("*.sparql"))
    if not queries:
        print(f"No queries in {args.query_dir}", file=sys.stderr)
        sys.exit(1)

    ok_ms, n_to, n_err = [], 0, 0
    csv_rows = []
    for qf in queries:
        query = qf.read_text().rstrip()
        # WDBench methodology: cap output per query at 100k rows. Our executor
        # pushes the LIMIT into the BGP join (early termination).
        if args.out_limit and "limit" not in query.lower():
            query = f"{query}\nLIMIT {args.out_limit}"
        status, ms, nres = run_query(args.endpoint_url, query, args.timeout)
        if status == "OK":
            ok_ms.append(ms)
        elif status == "TIMEOUT":
            n_to += 1
        else:
            n_err += 1
        csv_rows.append(f"{qf.name},{status},{ms:.1f},{nres}")

    if args.csv:
        Path(args.csv).write_text("query,status,ms,results\n" + "\n".join(csv_rows) + "\n")

    def q(p):
        s = sorted(ok_ms)
        return s[min(len(s) - 1, int(len(s) * p))] if s else float("nan")

    total = len(queries)
    if ok_ms:
        med, avg, p25, p75 = statistics.median(ok_ms), statistics.mean(ok_ms), q(0.25), q(0.75)
        line = (f"med={med:8.0f} avg={avg:9.0f} p25={p25:8.0f} p75={p75:8.0f} "
                f"ok={len(ok_ms):4} TIMEOUT={n_to:3} ERROR={n_err:3}  (n={total})")
    else:
        line = f"no OK queries  TIMEOUT={n_to} ERROR={n_err} (n={total})"
    print(f"{args.label:14} {Path(args.query_dir).name:14} {line}")


if __name__ == "__main__":
    main()
