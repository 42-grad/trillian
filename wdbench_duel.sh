#!/usr/bin/env bash
# wdbench_duel.sh — voller WDBench-Vergleich Rust-Clone vs. C++ Tentris auf
# ECHTEN Wikidata-Truthy-Daten (1.257.169.959 Tripel). Läuft NUR auf einer
# Big-RAM-Box (≥256 GB für beide Engines); vorher mit ./wdbench_probe.sh die
# Machbarkeit prüfen.
#
# Lädt den WDBench-Dump (Figshare, 3.6 GB .tar.bz2), erzeugt aus den
# WDBench-Query-Logs ausführbare .rq je Kategorie und vergleicht beide Engines
# pro Kategorie (Korrektheit als Multimenge + Latenz). Property Paths + C2RPQs
# sind seit dem Feature-Ausbau abgedeckt.
#
# Aufruf:  ./wdbench_duel.sh [stride] [max_per_category]
#   stride=1 (default) -> Volldatensatz. stride=N nimmt jede N-te Zeile
#            (kleinerer, aber realer Lauf; Queries treffen dann weniger Entities).
#   max_per_category default 100 (begrenzt die Vergleichslast je Kategorie).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_URL="https://ndownloader.figshare.com/files/42078477"
REPO_RAW="https://raw.githubusercontent.com/MillenniumDB/WDBench/master/Queries"
STRIDE="${1:-1}"
MAXQ="${2:-100}"

DATADIR="${ROOT}/wdbench_data"
QSRC="${ROOT}/wdbench_qsrc"
QDIR="${ROOT}/wdbench_queries_full"
FULL="${DATADIR}/wdbench_full.nt"
NT="${DATADIR}/wdbench_run.nt"
SNAP="${DATADIR}/wdbench.bin"
TDATA="${DATADIR}/wdbench-tentris-data"
RUST_PORT=9081
TENTRIS_PORT=9080
mkdir -p "${DATADIR}" "${QSRC}"

log() { echo "[wdbench] $*"; }
now_ms() { date +%s%3N; }
wait_port() { for _ in $(seq 1 600); do (echo > "/dev/tcp/localhost/$1") 2>/dev/null && return 0; sleep 1; done; return 1; }

# --- 1. Daten holen (einmalig dekomprimieren) -------------------------------
if [ ! -f "${FULL}" ]; then
    log "Lade + dekomprimiere WDBench-Dump (3.6 GB -> ~ vielfaches als .nt)..."
    curl -sL "${DATA_URL}" | tar -xjOf - > "${FULL}"
fi
if [ "${STRIDE}" = "1" ]; then
    NT="${FULL}"
else
    [ -f "${NT}" ] || awk "NR % ${STRIDE} == 1" "${FULL}" > "${NT}"
fi
log "Datensatz: $(wc -l < "${NT}") Tripel (stride=${STRIDE})"
TRIPLES="$(wc -l < "${NT}" | tr -d ' ')"

# --- 2. Query-Logs -> .rq je Kategorie --------------------------------------
for f in single_bgps multiple_bgps opts paths c2rpqs; do
    [ -f "${QSRC}/${f}.txt" ] || curl -sL -o "${QSRC}/${f}.txt" "${REPO_RAW}/${f}.txt"
done
python3 "${ROOT}/wdbench_queries.py" "${QSRC}" "${QDIR}" "${MAXQ}"

# --- 3. Rust-Clone: build + mmap-load (gemessen) ----------------------------
log "Baue Rust-Server + Snapshot..."
( cd "${ROOT}" && cargo build --release --bin server >/dev/null 2>&1 )
R_T0=$(now_ms)
"${ROOT}/target/release/server" build "${NT}" "${SNAP}" >/dev/null
"${ROOT}/target/release/server" load "${SNAP}" "${RUST_PORT}" >/tmp/rust_srv.log 2>&1 &
RUST_PID=$!
wait_port "${RUST_PORT}" || { log "Rust nicht bereit"; cat /tmp/rust_srv.log; exit 1; }
RUST_INGEST_MS=$(( $(now_ms) - R_T0 ))

# --- 4. Tentris: loader + server (gemessen) ---------------------------------
LOADER="$(find "${ROOT}/third_party/tentris/build" -name tentris_loader -type f | head -1)"
SERVER="$(find "${ROOT}/third_party/tentris/build" -name tentris_server -type f | head -1)"
if [ -z "${LOADER}" ] || [ -z "${SERVER}" ]; then
    log "FEHLER: Tentris-Binaries nicht gefunden (third_party/tentris/build)."
    kill "${RUST_PID}" 2>/dev/null || true; exit 1
fi
log "Tentris-Ingest..."
rm -rf "${TDATA}"; mkdir -p "${TDATA}"
T_T0=$(now_ms)
"${LOADER}" -f "${NT}" -s "${TDATA}" >/tmp/tentris_load.log 2>&1
"${SERVER}" -s "${TDATA}" -p "${TENTRIS_PORT}" >/tmp/tentris_srv.log 2>&1 &
TENTRIS_PID=$!
wait_port "${TENTRIS_PORT}" || { log "Tentris nicht bereit"; exit 1; }
TENTRIS_INGEST_MS=$(( $(now_ms) - T_T0 ))

cleanup() { kill "${RUST_PID}" "${TENTRIS_PID}" 2>/dev/null || true; }
trap cleanup EXIT

# --- 5. Vergleich je Kategorie ----------------------------------------------
echo
echo "=================== WDBench-Duell (${TRIPLES} Tripel) ==================="
for cat in single_bgps multiple_bgps opts paths c2rpqs; do
    echo
    echo "########## Kategorie: ${cat} ##########"
    python3 "${ROOT}/correctness_duel.py" \
        "http://localhost:${RUST_PORT}/sparql" \
        "http://localhost:${TENTRIS_PORT}/sparql" \
        "${QDIR}/${cat}" \
        --perf 5 || true
done

# --- 6. Ingest + Footprint (einmal, am Ende) --------------------------------
echo
echo "########## Ingest + Footprint ##########"
echo "  Ingest:  Rust ${RUST_INGEST_MS} ms | Tentris ${TENTRIS_INGEST_MS} ms"
DISK_R=$(du -m "${SNAP}" | cut -f1); DISK_T=$(du -m "${TDATA}" | cut -f1)
echo "  Disk:    Rust ${DISK_R} MB (Snapshot) | Tentris ${DISK_T} MB"
for label in "Rust ${RUST_PID}" "Tentris ${TENTRIS_PID}"; do
    name=${label% *}; pid=${label#* }
    if [ -r "/proc/${pid}/status" ]; then
        rss=$(awk '/VmRSS:/{print $2}' "/proc/${pid}/status")
        peak=$(awk '/VmHWM:/{print $2}' "/proc/${pid}/status")
        awk -v n="$name" -v r="$rss" -v p="$peak" -v t="$TRIPLES" 'BEGIN{
            printf "  RSS:     %-8s %.0f MB (Peak %.0f MB)  = %.1f B/Triple\n",
                   n, r/1024, p/1024, r*1024/t }'
    fi
done
log "Duell fertig."
