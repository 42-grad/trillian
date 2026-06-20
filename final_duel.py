#!/usr/bin/env python3
"""
final_duel.py  (v2)

Realistisches Duell: Rust SPARQL-Endpoint vs. C++ Tentris SPARQL-Endpoint.

Gegenüber v1 neu:
  * Graph-förmige Daten (gemeinsames S/O-Vokabular) -> Joins liefern echte,
    nicht-leere Ergebnisse (v1 mass nur den "finde nichts"-Pfad: 0 rows).
  * Mehr Query-Formen: Chain, Triangle (WCOJ), Star, DISTINCT, OPTIONAL.
  * Update-Throughput (INSERT/DELETE DATA via /update).
  * Keep-Alive HTTP-Client (statt 1000x neue Verbindung) + Median/p95 + cold/warm.
  * Memory-Footprint: Peak-RSS (/proc), Disk-Store-Größe, Bytes/Triple.

Unterstützte Tentris-Varianten:
  - Forschungsversion (dice-group/tentris): getrennte Binaries
  - Kommerzielle Beta (tentris/tentris): einheitliches Binary
"""

import http.client
import json
import shutil
import socket
import statistics
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import urlencode

PROJECT_ROOT = Path(__file__).resolve().parent
NT_FILE = PROJECT_ROOT / "synthetic_1m.nt"
CHAIN_QUERY_FILE = PROJECT_ROOT / "chain_query.sparql"
TRIANGLE_QUERY_FILE = PROJECT_ROOT / "triangle_query.sparql"

TENTRIS_DIR = PROJECT_ROOT / "third_party" / "tentris"
TENTRIS_BUILD_DIR = TENTRIS_DIR / "build"
COMMERCIAL_TENTRIS = TENTRIS_DIR / "tentris"
TENTRIS_DATA_DIR = PROJECT_ROOT / "tentris-data"

RUST_HOST, RUST_PORT = "localhost", 9081
TENTRIS_HOST, TENTRIS_PORT = "localhost", 9080

NS = "http://example.org/"

# Warm-Durchläufe pro Query (cold = erster Aufruf separat).
WARM_RUNS = 200
# Anzahl Tripel für den Update-Throughput-Benchmark.
UPDATE_BATCH = 20_000

# SPARQL-Queries. Alle nutzen Prädikate, die der graph-förmige Generator
# (src/synthetic.rs) mit echten Treffern bestückt.
QUERIES = {
    "chain": f"""PREFIX ex: <{NS}>
SELECT ?w ?x ?y ?z WHERE {{
  ?w ex:predicate_0 ?x .
  ?x ex:predicate_1 ?y .
  ?y ex:predicate_2 ?z .
}}""",
    "triangle": f"""PREFIX ex: <{NS}>
SELECT ?a ?b ?c WHERE {{
  ?a ex:predicate_0 ?b .
  ?b ex:predicate_0 ?c .
  ?c ex:predicate_0 ?a .
}}""",
    "star": f"""PREFIX ex: <{NS}>
SELECT ?s ?a ?b ?c WHERE {{
  ?s ex:predicate_0 ?a .
  ?s ex:predicate_1 ?b .
  ?s ex:predicate_2 ?c .
}}""",
    "distinct": f"""PREFIX ex: <{NS}>
SELECT DISTINCT ?o WHERE {{
  ?s ex:predicate_0 ?o .
}}""",
    "optional": f"""PREFIX ex: <{NS}>
SELECT ?a ?b ?c WHERE {{
  ?a ex:predicate_0 ?b .
  OPTIONAL {{ ?b ex:predicate_0 ?c }}
}}""",
}


def log(msg: str) -> None:
    print(f"[final_duel] {msg}")


def run_command(cmd, cwd=None, timeout=None, stdin=None):
    log(f"Running: {' '.join(str(c) for c in cmd)}")
    return subprocess.run(
        cmd, cwd=cwd or PROJECT_ROOT, capture_output=True, text=True,
        timeout=timeout, stdin=stdin,
    )


def is_graph_shaped(path: Path) -> bool:
    """Prüft, ob die .nt-Datei das graph-förmige Vokabular (entity_*) nutzt.

    Alte Läufe erzeugten disjunkte Vokabulare (subject_*/object_*), bei denen
    Chain-/Triangle-Joins strukturell nie matchen. Solche Dateien müssen neu
    generiert werden, sonst misst das Duell wieder leere Ergebnisse.
    """
    try:
        with path.open("r") as f:
            for line in f:
                line = line.strip()
                if line and not line.startswith("#"):
                    return "/entity_" in line
    except OSError:
        return False
    return False


def ensure_synthetic_data() -> None:
    if NT_FILE.exists():
        if is_graph_shaped(NT_FILE):
            log(f"{NT_FILE.name} vorhanden und graph-förmig ({NT_FILE.stat().st_size / 1024 / 1024:.1f} MB).")
            return
        log(f"{NT_FILE.name} ist veraltet (kein entity_-Vokabular) – regeneriere...")
        NT_FILE.unlink()
    else:
        log("synthetic_1m.nt fehlt – generiere ...")
    # --bin explizit: seit dem server-Binary gibt es kein eindeutiges Default-Target.
    result = run_command(["cargo", "run", "--release", "--bin", "tentris_clone"], timeout=600)
    if result.returncode != 0 or not NT_FILE.exists():
        log("FEHLER: Daten-Generierung fehlgeschlagen.")
        print(result.stdout + result.stderr)
        sys.exit(1)
    if not is_graph_shaped(NT_FILE):
        log("FEHLER: Generierte Datei ist nicht graph-förmig (entity_-Vokabular fehlt).")
        sys.exit(1)
    log(f"Graph-förmige Daten erzeugt ({NT_FILE.stat().st_size / 1024 / 1024:.1f} MB).")


def write_sparql_queries() -> None:
    CHAIN_QUERY_FILE.write_text(QUERIES["chain"] + "\n")
    TRIANGLE_QUERY_FILE.write_text(QUERIES["triangle"] + "\n")
    log("SPARQL-Abfragen geschrieben (chain, triangle).")


def count_triples() -> int:
    with NT_FILE.open("rb") as f:
        return sum(1 for _ in f)


# ---------------------------------------------------------------------------
# HTTP-Client mit Keep-Alive
# ---------------------------------------------------------------------------


class KeepAliveClient:
    """Wiederverwendbare HTTP/1.1-Verbindung – misst Engine-Zeit statt
    TCP-Connect-Overhead pro Request."""

    def __init__(self, host: str, port: int):
        self.host = host
        self.port = port
        self.conn = http.client.HTTPConnection(host, port, timeout=120)

    def _reconnect(self):
        try:
            self.conn.close()
        except Exception:
            pass
        self.conn = http.client.HTTPConnection(self.host, self.port, timeout=120)

    def get_sparql(self, query: str):
        url = "/sparql?" + urlencode({"query": query})
        headers = {"Accept": "application/sparql-results+json"}
        for attempt in (0, 1):
            try:
                self.conn.request("GET", url, headers=headers)
                resp = self.conn.getresponse()
                return resp.status, resp.read()
            except (http.client.HTTPException, OSError):
                if attempt == 1:
                    raise
                self._reconnect()

    def post(self, path: str, data: str, ctype: str):
        body = data.encode("utf-8")
        headers = {"Content-Type": ctype}
        for attempt in (0, 1):
            try:
                self.conn.request("POST", path, body=body, headers=headers)
                resp = self.conn.getresponse()
                return resp.status, resp.read()
            except (http.client.HTTPException, OSError):
                if attempt == 1:
                    raise
                self._reconnect()


def count_rows(body: bytes):
    try:
        data = json.loads(body)
        if "boolean" in data:
            return 1 if data["boolean"] else 0
        return len(data.get("results", {}).get("bindings", []))
    except (json.JSONDecodeError, KeyError):
        return None


# ---------------------------------------------------------------------------
# Server-Lebenszyklus
# ---------------------------------------------------------------------------


def wait_for_port(host, port, timeout=120.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection((host, port), timeout=1.0):
                return True
        except OSError:
            time.sleep(0.2)
    return False


def port_is_free(host, port) -> bool:
    try:
        with socket.create_connection((host, port), timeout=0.5):
            return False
    except OSError:
        return True


def stop_server(proc, name: str) -> None:
    if proc is None:
        return
    log(f"Beende {name}-Server...")
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def build_rust_server() -> None:
    log("Baue Rust SPARQL-Server (cargo build --release --bin server)...")
    result = run_command(["cargo", "build", "--release", "--bin", "server"], timeout=600)
    if result.returncode != 0:
        log("FEHLER: Rust-Server-Build fehlgeschlagen.")
        print(result.stdout + result.stderr)
        sys.exit(1)


def start_rust_server():
    """Loader + Server analog zu Tentris: erst Index bauen + als mmap-Snapshot
    persistieren, dann den Server starten, der den Snapshot memory-mappt.
    Damit ist Ingest/Startup apples-to-apples (beide disk-backed/mmap)."""
    if not port_is_free(RUST_HOST, RUST_PORT):
        log(f"FEHLER: Port {RUST_PORT} ist bereits belegt.")
        sys.exit(1)
    server_bin = str(PROJECT_ROOT / "target" / "release" / "server")
    snapshot = PROJECT_ROOT / "rust-snapshot.bin"

    start = time.perf_counter()
    # Loader: Index bauen + persistieren.
    log("Rust-Loader: baue + persistiere Index (mmap-Snapshot)...")
    result = run_command([server_bin, "build", str(NT_FILE), str(snapshot)], timeout=600)
    if result.returncode != 0:
        log("FEHLER: Rust-Loader fehlgeschlagen.")
        print(result.stdout + result.stderr)
        sys.exit(1)

    # Server: Snapshot per mmap laden + serven.
    log(f"Starte Rust SPARQL-Server (mmap-load) auf Port {RUST_PORT}...")
    proc = subprocess.Popen(
        [server_bin, "load", str(snapshot), str(RUST_PORT)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    if not wait_for_port(RUST_HOST, RUST_PORT):
        log("Rust-Server hat Port nicht rechtzeitig geöffnet.")
        stop_server(proc, "Rust")
        sys.exit(1)
    ingest_ms = (time.perf_counter() - start) * 1000
    log(f"Rust-Server bereit (Loader+mmap-Startup: {ingest_ms:.1f} ms).")
    return proc, ingest_ms


def detect_tentris():
    loader = list(TENTRIS_BUILD_DIR.rglob("tentris_loader"))
    server = list(TENTRIS_BUILD_DIR.rglob("tentris_server"))
    if loader and server:
        return ("Forschungs-Tentris", loader[0], server[0])
    if COMMERCIAL_TENTRIS.exists():
        return ("Kommerzielles Tentris", COMMERCIAL_TENTRIS, COMMERCIAL_TENTRIS)
    return None


def clean_tentris_datastore():
    if TENTRIS_DATA_DIR.exists():
        shutil.rmtree(TENTRIS_DATA_DIR)


def start_tentris_server():
    flavor_info = detect_tentris()
    if flavor_info is None:
        log("FEHLER: Keine Tentris-Installation gefunden.")
        sys.exit(1)
    flavor_name, loader, server = flavor_info
    log(f"Verwende Tentris-Variante: {flavor_name}")
    if not port_is_free(TENTRIS_HOST, TENTRIS_PORT):
        log(f"FEHLER: Port {TENTRIS_PORT} ist bereits belegt.")
        sys.exit(1)

    start = time.perf_counter()
    if flavor_name.startswith("Kommerzielles"):
        clean_tentris_datastore()
        with NT_FILE.open("rb") as nt_stream:
            result = subprocess.run(
                [str(loader), "load", "--force-no-snapshot"], stdin=nt_stream,
                capture_output=True, text=True, cwd=PROJECT_ROOT, timeout=600,
            )
        if result.returncode != 0:
            log("Tentris-Ingest fehlgeschlagen.")
            print(result.stdout + result.stderr)
            sys.exit(1)
        proc = subprocess.Popen([str(server), "serve"], stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True, cwd=PROJECT_ROOT)
    else:
        clean_tentris_datastore()
        TENTRIS_DATA_DIR.mkdir(parents=True, exist_ok=True)
        result = run_command([str(loader), "-f", str(NT_FILE), "-s", str(TENTRIS_DATA_DIR)], timeout=600)
        if result.returncode != 0:
            log("Tentris-Ingest fehlgeschlagen.")
            print(result.stdout + result.stderr)
            sys.exit(1)
        proc = subprocess.Popen([str(server), "-s", str(TENTRIS_DATA_DIR), "-p", str(TENTRIS_PORT)],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    if not wait_for_port(TENTRIS_HOST, TENTRIS_PORT):
        log("Tentris-Server hat Port nicht rechtzeitig geöffnet.")
        stop_server(proc, "Tentris")
        sys.exit(1)
    ingest_ms = (time.perf_counter() - start) * 1000
    log(f"Tentris-Server bereit (Ingest/Startup: {ingest_ms:.1f} ms).")
    return proc, ingest_ms


# ---------------------------------------------------------------------------
# Memory-Footprint
# ---------------------------------------------------------------------------


def read_proc_status(pid: int):
    """Liest VmHWM (Peak-RSS) und VmRSS aus /proc/<pid>/status (nur Linux)."""
    try:
        with open(f"/proc/{pid}/status") as f:
            text = f.read()
    except OSError:
        return None
    out = {}
    for line in text.splitlines():
        if line.startswith("VmHWM:"):
            out["peak_rss_kb"] = int(line.split()[1])
        elif line.startswith("VmRSS:"):
            out["rss_kb"] = int(line.split()[1])
    return out or None


def dir_size_bytes(path: Path) -> int:
    if not path.exists():
        return 0
    if path.is_file():
        return path.stat().st_size
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file())


# ---------------------------------------------------------------------------
# Benchmarks
# ---------------------------------------------------------------------------


def benchmark_query(client: KeepAliveClient, query: str, warm_runs: int):
    """Cold (erster Aufruf = Cache-Miss) + Warm-Verteilung (Median/p95)."""
    t0 = time.perf_counter()
    status, body = client.get_sparql(query)
    cold_ms = (time.perf_counter() - t0) * 1000
    rows = count_rows(body)

    lat = []
    for _ in range(warm_runs):
        t0 = time.perf_counter()
        client.get_sparql(query)
        lat.append((time.perf_counter() - t0) * 1000)
    lat.sort()
    p95 = lat[max(0, int(len(lat) * 0.95) - 1)]
    return {
        "rows": rows, "status": status, "cold_ms": cold_ms,
        "warm_median_ms": statistics.median(lat), "warm_p95_ms": p95,
    }


def build_update(verb: str, k: int, base: int = 9_000_000) -> str:
    lines = [
        f"<{NS}entity_{base + i}> <{NS}predicate_0> <{NS}entity_{base + i + 1}> ."
        for i in range(k)
    ]
    return f"{verb} {{ " + "\n".join(lines) + " }"


def benchmark_updates(client: KeepAliveClient, update_path: str, k: int):
    """Insert- und Delete-Throughput in Triples/Sekunde."""
    insert = build_update("INSERT DATA", k)
    t0 = time.perf_counter()
    status_i, _ = client.post(update_path, insert, "application/sparql-update")
    insert_s = time.perf_counter() - t0

    delete = build_update("DELETE DATA", k)
    t0 = time.perf_counter()
    status_d, _ = client.post(update_path, delete, "application/sparql-update")
    delete_s = time.perf_counter() - t0

    ok = status_i in (200, 204) and status_d in (200, 204)
    return {
        "ok": ok,
        "insert_tps": k / insert_s if insert_s > 0 else None,
        "delete_tps": k / delete_s if delete_s > 0 else None,
        "insert_ms": insert_s * 1000,
        "delete_ms": delete_s * 1000,
        "status": (status_i, status_d),
    }


def benchmark_endpoint(host, port, name, update_path):
    client = KeepAliveClient(host, port)
    results = {"queries": {}}
    for qname, query in QUERIES.items():
        log(f"[{name}] Query '{qname}' (cold + {WARM_RUNS} warm)...")
        try:
            results["queries"][qname] = benchmark_query(client, query, WARM_RUNS)
        except Exception as e:
            log(f"[{name}] Query '{qname}' fehlgeschlagen: {e}")
            results["queries"][qname] = None
    log(f"[{name}] Update-Throughput ({UPDATE_BATCH} Triples)...")
    try:
        results["updates"] = benchmark_updates(client, update_path, UPDATE_BATCH)
    except Exception as e:
        log(f"[{name}] Update-Benchmark fehlgeschlagen: {e}")
        results["updates"] = None
    return results


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------


def fmt(v, unit, prec=2):
    return "n/a" if v is None else f"{v:.{prec}f} {unit}"


def winner_lower(a, b):
    if a is None or b is None:
        return "—"
    if a == b:
        return "gleich"
    return f"Rust ({b / a:.1f}x)" if a < b else f"Tentris ({a / b:.1f}x)"


def winner_higher(a, b):
    if a is None or b is None:
        return "—"
    if a == b:
        return "gleich"
    return f"Rust ({a / b:.1f}x)" if a > b else f"Tentris ({b / a:.1f}x)"


def print_report(n_triples, rust_ingest, tentris_ingest, rust, tentris, rust_mem, tentris_mem, flavor):
    print("\n" + "=" * 90)
    print(f"FINALES DUELL v2: Rust SPARQL-Endpoint vs. C++ Tentris ({flavor})")
    print(f"Datensatz: {n_triples} Triples (graph-förmig)")
    print("=" * 90)

    print("\n### Ingest / Startup")
    print("| Metrik | Rust | Tentris | Gewinner |")
    print("| :-- | :-- | :-- | :-- |")
    print(f"| Ingest+Startup | {fmt(rust_ingest,'ms')} | {fmt(tentris_ingest,'ms')} | {winner_lower(rust_ingest, tentris_ingest)} |")

    print("\n### Query-Latenz (Warm-Median / p95 / Cold) + Konsistenz")
    print("| Query | Rust median | Rust p95 | Rust cold | Tentris median | Tentris p95 | Tentris cold | Rows R/T | Gewinner(median) |")
    print("| :-- | --: | --: | --: | --: | --: | --: | :--: | :-- |")
    for q in QUERIES:
        r = rust["queries"].get(q)
        t = tentris["queries"].get(q)
        rm = r["warm_median_ms"] if r else None
        tm = t["warm_median_ms"] if t else None
        rr = r["rows"] if r else None
        tr = t["rows"] if t else None
        consist = "OK" if (rr is not None and rr == tr) else "DIFF"
        print(
            f"| {q} | {fmt(rm,'ms',3)} | {fmt(r['warm_p95_ms'] if r else None,'ms',3)} | {fmt(r['cold_ms'] if r else None,'ms',2)} | "
            f"{fmt(tm,'ms',3)} | {fmt(t['warm_p95_ms'] if t else None,'ms',3)} | {fmt(t['cold_ms'] if t else None,'ms',2)} | "
            f"{rr}/{tr} {consist} | {winner_lower(rm, tm)} |"
        )

    print("\n### Update-Throughput (INSERT/DELETE DATA)")
    ru = rust.get("updates")
    tu = tentris.get("updates")
    print("| Operation | Rust | Tentris | Gewinner |")
    print("| :-- | --: | --: | :-- |")
    ri = ru["insert_tps"] if ru else None
    ti = tu["insert_tps"] if tu else None
    rd = ru["delete_tps"] if ru else None
    td = tu["delete_tps"] if tu else None
    print(f"| INSERT (triples/s) | {fmt(ri,'/s',0)} | {fmt(ti,'/s',0)} | {winner_higher(ri, ti)} |")
    print(f"| DELETE (triples/s) | {fmt(rd,'/s',0)} | {fmt(td,'/s',0)} | {winner_higher(rd, td)} |")

    print("\n### Memory-Footprint")
    print("| Metrik | Rust (in-memory) | Tentris (mmap/disk) |")
    print("| :-- | --: | --: |")
    r_peak = rust_mem.get("peak_rss_kb") if rust_mem else None
    t_peak = tentris_mem.get("peak_rss_kb") if tentris_mem else None
    r_rss = rust_mem.get("rss_kb") if rust_mem else None
    t_rss = tentris_mem.get("rss_kb") if tentris_mem else None
    r_disk = rust_mem.get("disk_bytes") if rust_mem else None
    t_disk = tentris_mem.get("disk_bytes") if tentris_mem else None
    mb = lambda kb: None if kb is None else kb / 1024
    mbb = lambda b: None if b is None else b / 1024 / 1024
    print(f"| Peak-RSS (VmHWM) | {fmt(mb(r_peak),'MB')} | {fmt(mb(t_peak),'MB')} |")
    print(f"| RSS (VmRSS) | {fmt(mb(r_rss),'MB')} | {fmt(mb(t_rss),'MB')} |")
    print(f"| Disk-Store | {fmt(mbb(r_disk),'MB')} (.nt-Quelle) | {fmt(mbb(t_disk),'MB')} (metall) |")
    if r_peak:
        print(f"| Bytes/Triple (RSS) | {fmt(r_peak * 1024 / n_triples,'B',1)} | {fmt(t_peak * 1024 / n_triples if t_peak else None,'B',1)} |")
    print(
        "\nHinweis: Tentris ist disk-/mmap-basiert – VmRSS kann den realen "
        "Footprint unterschätzen; der metall-Store auf Disk ist die ehrlichere "
        "Größe. Der Rust-Klon hält den Index komplett im RAM (3 Permutationen + "
        "Prädikat-Relationen), daher ist RSS dort die maßgebliche Größe."
    )


# ---------------------------------------------------------------------------
# Hauptablauf
# ---------------------------------------------------------------------------


def main() -> None:
    ensure_synthetic_data()
    write_sparql_queries()
    n_triples = count_triples()
    log(f"Datensatz: {n_triples} Triples.")
    build_rust_server()

    update_path = "/update"  # Forschungs-Tentris und Rust-Klon nutzen /update

    rust_proc, rust_ingest = start_rust_server()
    try:
        tentris_proc, tentris_ingest = start_tentris_server()
        try:
            rust_results = benchmark_endpoint(RUST_HOST, RUST_PORT, "rust", update_path)
            tentris_results = benchmark_endpoint(TENTRIS_HOST, TENTRIS_PORT, "tentris", update_path)

            # Memory am Ende erfassen (VmHWM = Peak über die gesamte Laufzeit).
            rust_mem = read_proc_status(rust_proc.pid) or {}
            rust_mem["disk_bytes"] = dir_size_bytes(NT_FILE)
            tentris_mem = read_proc_status(tentris_proc.pid) or {}
            tentris_mem["disk_bytes"] = dir_size_bytes(TENTRIS_DATA_DIR)

            flavor = detect_tentris()[0] if detect_tentris() else "unbekannt"
            print_report(n_triples, rust_ingest, tentris_ingest, rust_results,
                         tentris_results, rust_mem, tentris_mem, flavor)
        finally:
            stop_server(tentris_proc, "Tentris")
    finally:
        stop_server(rust_proc, "Rust")


if __name__ == "__main__":
    main()
