#!/usr/bin/env bash
# watdiv_correctness.sh — Korrektheits- + Performance-Vergleich auf ECHTEN
# (WatDiv-)Daten: baut WatDiv, generiert Daten + reale BGP-Queries, lädt beide
# Engines (Rust-Clone + C++ Tentris) und vergleicht die vollständigen
# Binding-Mengen via correctness_duel.py.
#
# Läuft auf der Linux-Box (Tentris muss unter third_party/tentris/build gebaut
# sein, wie nach run_remote_duel.sh). Aufruf:
#     ./watdiv_correctness.sh [scale-factor]   # default 10  (~1M Triples)
#
# Voraussetzungen: g++, make, libboost-date-time-dev, python3, cargo.
set -euo pipefail

SCALE="${1:-10}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WD="${ROOT}/third_party/watdiv"
NT="${ROOT}/watdiv_sf${SCALE}.nt"
QDIR="${ROOT}/watdiv_queries"
SNAP="${ROOT}/watdiv.bin"
TDATA="${ROOT}/watdiv-tentris-data"
RUST_PORT=9081
TENTRIS_PORT=9080

log() { echo "[watdiv] $*"; }

# --- 1. WatDiv-Generator bauen (mit Fixes für modernes Boost/C++17) ---------
if [ ! -x "${WD}/bin/Release/watdiv" ]; then
    log "Baue WatDiv-Generator..."
    command -v apt-get >/dev/null && sudo apt-get install -y libboost-date-time-dev >/dev/null 2>&1 || true
    mkdir -p "${WD}"
    curl -sL -o /tmp/watdiv_v06.tar https://dsg.uwaterloo.ca/watdiv/watdiv_v06.tar
    tar -xf /tmp/watdiv_v06.tar -C "${WD}" --strip-components=1
    cd "${WD}"
    sed -i 's/c++0x/c++17/g' Makefile
    # std::random_shuffle wurde in C++17 entfernt -> deterministisches std::shuffle
    perl -0pi -e 's/random_shuffle\(eligible_list\.begin\(\), eligible_list\.end\(\)\);/{ static std::mt19937 _rng(42); std::shuffle(eligible_list.begin(), eligible_list.end(), _rng); }/' src/statistics.cpp
    grep -q '#include <random>' src/statistics.cpp || sed -i '1i #include <random>' src/statistics.cpp
    make
    cd "${ROOT}"
fi

# --- 2. Daten generieren ----------------------------------------------------
if [ ! -f "${NT}" ]; then
    log "Generiere WatDiv-Daten (scale ${SCALE})..."
    "${WD}/bin/Release/watdiv" -d "${WD}/model/wsdbm-data-model.txt" "${SCALE}" > "${NT}"
fi
log "Datensatz: $(wc -l < "${NT}") Tripel"

# --- 3. Reale BGP-Queries aus den Daten erzeugen ----------------------------
python3 "${ROOT}/watdiv_queries.py" "${NT}" "${QDIR}"

# --- 4. Rust-Clone: Snapshot bauen + Server starten -------------------------
log "Baue Rust-Server + Snapshot..."
( cd "${ROOT}" && cargo build --release --bin server >/dev/null 2>&1 )
"${ROOT}/target/release/server" build "${NT}" "${SNAP}" >/dev/null
"${ROOT}/target/release/server" load "${SNAP}" "${RUST_PORT}" >/tmp/rust_srv.log 2>&1 &
RUST_PID=$!

# --- 5. Tentris: laden + Server starten -------------------------------------
LOADER="$(find "${ROOT}/third_party/tentris/build" -name tentris_loader -type f | head -1)"
SERVER="$(find "${ROOT}/third_party/tentris/build" -name tentris_server -type f | head -1)"
if [ -z "${LOADER}" ] || [ -z "${SERVER}" ]; then
    log "FEHLER: Tentris-Binaries nicht gefunden."
    kill "${RUST_PID}" 2>/dev/null || true
    exit 1
fi
log "Tentris-Ingest..."
rm -rf "${TDATA}"; mkdir -p "${TDATA}"
"${LOADER}" -f "${NT}" -s "${TDATA}" >/tmp/tentris_load.log 2>&1
"${SERVER}" -s "${TDATA}" -p "${TENTRIS_PORT}" >/tmp/tentris_srv.log 2>&1 &
TENTRIS_PID=$!

cleanup() { kill "${RUST_PID}" "${TENTRIS_PID}" 2>/dev/null || true; }
trap cleanup EXIT

# --- 6. Auf beide Ports warten ----------------------------------------------
for port in "${RUST_PORT}" "${TENTRIS_PORT}"; do
    for _ in $(seq 1 120); do
        if (echo > "/dev/tcp/localhost/${port}") 2>/dev/null; then break; fi
        sleep 1
    done
done

# --- 7. Korrektheit + Performance vergleichen -------------------------------
log "Vergleiche Binding-Mengen (Rust :${RUST_PORT} vs Tentris :${TENTRIS_PORT})..."
python3 "${ROOT}/correctness_duel.py" \
    "http://localhost:${RUST_PORT}/sparql" \
    "http://localhost:${TENTRIS_PORT}/sparql" \
    "${QDIR}" --perf 50 | tee "${ROOT}/watdiv_correctness.log"
