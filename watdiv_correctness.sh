#!/usr/bin/env bash
# watdiv_correctness.sh — Korrektheits- + Performance-Vergleich auf ECHTEN
# WatDiv-Daten: lädt einen **vorgenerierten** WatDiv-10M-Dump (kein brüchiger
# Generator-Build), erzeugt reale BGP-Queries, lädt beide Engines (Rust-Clone +
# C++ Tentris) und vergleicht die vollständigen Binding-Mengen via
# correctness_duel.py.
#
# Läuft auf der Linux-Box (Tentris unter third_party/tentris/build gebaut).
# Aufruf:  ./watdiv_correctness.sh [stride]   # default 4 -> ~2.7M Triples
#          stride=N nimmt jede N-te Zeile (diverse Auswahl, beschränkt Größe).
set -euo pipefail

STRIDE="${1:-4}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WATDIV_URL="https://dsg.uwaterloo.ca/watdiv/watdiv.10M.tar.bz2"
FULL="${ROOT}/watdiv.10M.nt"
NT="${ROOT}/watdiv_slice.nt"
QDIR="${ROOT}/watdiv_queries"
SNAP="${ROOT}/watdiv.bin"
TDATA="${ROOT}/watdiv-tentris-data"
RUST_PORT=9081
TENTRIS_PORT=9080

log() { echo "[watdiv] $*"; }

# --- 1. Vorgenerierten WatDiv-Dump laden (kein Generator-Build) -------------
if [ ! -f "${FULL}" ]; then
    log "Lade vorgenerierten WatDiv-10M-Dump (~56 MB)..."
    curl -sL -o /tmp/watdiv.10M.tar.bz2 "${WATDIV_URL}"
    tar -xjf /tmp/watdiv.10M.tar.bz2 -C "${ROOT}" watdiv.10M.nt
fi

# --- 2. Diverse, beschränkte Slice (jede STRIDE-te Zeile) -------------------
if [ ! -f "${NT}" ]; then
    log "Erzeuge Slice (jede ${STRIDE}. Zeile)..."
    awk "NR % ${STRIDE} == 1" "${FULL}" > "${NT}"
fi
log "Datensatz: $(wc -l < "${NT}") Tripel (echte WatDiv-Daten)"

# --- 3. Reale BGP-Queries aus den Daten erzeugen ----------------------------
python3 "${ROOT}/watdiv_queries.py" "${NT}" "${QDIR}"

now_ms() { date +%s%3N; }
wait_port() { for _ in $(seq 1 300); do (echo > "/dev/tcp/localhost/$1") 2>/dev/null && return 0; sleep 1; done; return 1; }
TRIPLES="$(wc -l < "${NT}" | tr -d ' ')"

# --- 4. Rust-Clone: Loader (build+persist) + mmap-Server (gemessen) ---------
log "Baue Rust-Server + Snapshot..."
( cd "${ROOT}" && cargo build --release --bin server >/dev/null 2>&1 )
R_T0=$(now_ms)
"${ROOT}/target/release/server" build "${NT}" "${SNAP}" >/dev/null
"${ROOT}/target/release/server" load "${SNAP}" "${RUST_PORT}" >/tmp/rust_srv.log 2>&1 &
RUST_PID=$!
wait_port "${RUST_PORT}" || { log "Rust-Server nicht bereit"; kill "${RUST_PID}" 2>/dev/null || true; exit 1; }
RUST_INGEST_MS=$(( $(now_ms) - R_T0 ))

# --- 5. Tentris: Loader + Server (gemessen) ---------------------------------
LOADER="$(find "${ROOT}/third_party/tentris/build" -name tentris_loader -type f | head -1)"
SERVER="$(find "${ROOT}/third_party/tentris/build" -name tentris_server -type f | head -1)"
if [ -z "${LOADER}" ] || [ -z "${SERVER}" ]; then
    log "FEHLER: Tentris-Binaries nicht gefunden."
    kill "${RUST_PID}" 2>/dev/null || true
    exit 1
fi
log "Tentris-Ingest..."
rm -rf "${TDATA}"; mkdir -p "${TDATA}"
T_T0=$(now_ms)
"${LOADER}" -f "${NT}" -s "${TDATA}" >/tmp/tentris_load.log 2>&1
"${SERVER}" -s "${TDATA}" -p "${TENTRIS_PORT}" >/tmp/tentris_srv.log 2>&1 &
TENTRIS_PID=$!
wait_port "${TENTRIS_PORT}" || { log "Tentris-Server nicht bereit"; exit 1; }
TENTRIS_INGEST_MS=$(( $(now_ms) - T_T0 ))

cleanup() { kill "${RUST_PID}" "${TENTRIS_PID}" 2>/dev/null || true; }
trap cleanup EXIT

# --- 6. Vollständiger Vergleich: Korrektheit + Latenz + Updates + Memory ----
log "Vergleiche (Korrektheit + Latenz + Updates + Footprint)..."
# Report, kein Gate -> Exit-Code nicht propagieren.
python3 "${ROOT}/correctness_duel.py" \
    "http://localhost:${RUST_PORT}/sparql" \
    "http://localhost:${TENTRIS_PORT}/sparql" \
    "${QDIR}" \
    --perf 50 --update 20000 --triples "${TRIPLES}" \
    --rust-pid "${RUST_PID}" --tentris-pid "${TENTRIS_PID}" \
    --rust-disk "${SNAP}" --tentris-disk "${TDATA}" \
    --ingest-rust "${RUST_INGEST_MS}" --ingest-tentris "${TENTRIS_INGEST_MS}" || true
