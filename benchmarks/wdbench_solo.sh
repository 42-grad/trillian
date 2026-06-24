#!/usr/bin/env bash
# wdbench_solo.sh — WDBench run for our engine, in the published format, for the
# absolute comparison with the official Blazegraph/Jena/Virtuoso/Neo4j numbers
# (Results/*.xlsx, 60-s timeout, ms).
#
# Downloads the WDBench dump, decompresses it FULLY (bzip2, since lbzip2
# truncates this file at ~493M lines), builds our snapshot, loads the server,
# and measures all five query classes (complete sets, no cap) with wdbench_bench.py.
#
# Usage:  ./wdbench_solo.sh [stride] [timeout_s]
#   stride=1 (default) -> full 1.257-billion-triple graph (for the real comparison).
#   timeout_s default 60 (like the reference).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # benchmarks/
ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"                       # project root
# Canonical, COMPLETE WDBench dump (9.15 GB .nt.bz2 -> ~1.257 billion triples).
# NOT the truncated latest_truthy_data_filtered.tar.bz2 (yields only ~495M).
DATA_URL="https://ndownloader.figshare.com/files/34816081"
REPO_RAW="https://raw.githubusercontent.com/MillenniumDB/WDBench/master/Queries"
EXPECTED_MD5="b3ef85c9106100808a7e6e9315326059"
STRIDE="${1:-1}"
TIMEOUT="${2:-60}"

DATADIR="${ROOT}/wdbench_data"
QSRC="${ROOT}/wdbench_qsrc"
QDIR="${ROOT}/wdbench_queries_solo"
ARCHIVE="${DATADIR}/wdbench.nt.bz2"
FULL="${DATADIR}/wdbench_full.nt"
NT="${DATADIR}/wdbench_solo.nt"
SNAP="${DATADIR}/wdbench_solo.bin"
PORT=9081
mkdir -p "${DATADIR}" "${QSRC}"
log() { echo "[solo] $*"; }
now_ms() { date +%s%3N; }
wait_port() { for _ in $(seq 1 600); do (echo > "/dev/tcp/localhost/$1") 2>/dev/null && return 0; sleep 1; done; return 1; }
md5of() { md5sum "$1" 2>/dev/null | awk '{print $1}'; }

# --- 1. Data: download (MD5) + decompress (plain .nt.bz2, no tar) -----------
if [ ! -s "${FULL}" ]; then
    if [ ! -s "${ARCHIVE}" ] || [ "$(md5of "${ARCHIVE}")" != "${EXPECTED_MD5}" ]; then
        log "Downloading the complete WDBench dump (9.15 GB)..."
        rm -f "${ARCHIVE}"
        curl -L --fail --retry 6 --retry-delay 5 -o "${ARCHIVE}" "${DATA_URL}"
        [ "$(md5of "${ARCHIVE}")" = "${EXPECTED_MD5}" ] && log "MD5 ok." || log "WARN: MD5 mismatch."
    fi
    # bzip2 (single-threaded, ~40-60 min, but reliable; verified on this complete
    # file). Plain .nt.bz2 -> NO tar. Fail-loud: a decompress error aborts instead
    # of silently using a partial file.
    log "Decompressing (bzip2) -> ${FULL} ..."
    bzip2 -dc "${ARCHIVE}" > "${FULL}.partial"
    mv "${FULL}.partial" "${FULL}"
fi
FULL_LINES=$(wc -l < "${FULL}" | tr -d ' ')
log "Decompressed: ${FULL_LINES} lines"
if [ "${STRIDE}" = "1" ]; then NT="${FULL}"; else
    [ -s "${NT}" ] || awk "NR % ${STRIDE} == 1" "${FULL}" > "${NT}"
fi
TRIPLES=$(wc -l < "${NT}" | tr -d ' ')
log "Dataset: ${TRIPLES} triples (stride=${STRIDE})"

# --- 2. Query logs -> .rq (COMPLETE sets, no cap) ---------------------------
for f in single_bgps multiple_bgps opts paths c2rpqs; do
    [ -f "${QSRC}/${f}.txt" ] || curl -sL -o "${QSRC}/${f}.txt" "${REPO_RAW}/${f}.txt"
done
python3 "${SCRIPT_DIR}/wdbench_queries.py" "${QSRC}" "${QDIR}"

# --- 3. Our engine: build + mmap load (measured) ----------------------------
log "Building snapshot + loading server..."
export PATH="${PATH}:/root/.cargo/bin:${HOME:-/root}/.cargo/bin"
[ -x "${ROOT}/target/release/server" ] || ( cd "${ROOT}" && cargo build --release --bin server >/dev/null 2>&1 )
B0=$(now_ms)
"${ROOT}/target/release/server" build "${NT}" "${SNAP}" >/dev/null
"${ROOT}/target/release/server" load "${SNAP}" "${PORT}" >/tmp/solo_srv.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
wait_port "${PORT}" || { log "Server not ready"; cat /tmp/solo_srv.log; exit 1; }
INGEST_MS=$(( $(now_ms) - B0 ))

# --- 4. Benchmark per class (published format) ------------------------------
echo
echo "=========== Trillian WDBench (${TRIPLES} triples, ${TIMEOUT}s timeout) ==========="
RSSKB=$(awk '/VmRSS/{print $2}' "/proc/${SRV}/status" 2>/dev/null || echo "")
echo "Ingest+Load: ${INGEST_MS} ms | Snapshot disk: $(du -m "${SNAP}" 2>/dev/null | cut -f1) MB | RSS: ${RSSKB:-n/a} KB"
echo "Label          Category       Aggregate (ms)"
echo "-------------------------------------------------------------------------------"
for cat in single_bgps multiple_bgps opts paths c2rpqs; do
    python3 "${SCRIPT_DIR}/wdbench_bench.py" "http://localhost:${PORT}/sparql" "${QDIR}/${cat}" \
        --timeout "${TIMEOUT}" --label trillian --csv "${DATADIR}/solo_${cat}.csv" || true
done
log "Done. Per-query CSVs in ${DATADIR}/solo_*.csv"
