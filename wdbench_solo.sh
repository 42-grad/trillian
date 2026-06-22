#!/usr/bin/env bash
# wdbench_solo.sh — Tentris-FREIER WDBench-Lauf nur für unsere Engine, im
# publizierten Format, für den absoluten Vergleich mit den offiziellen
# Blazegraph/Jena/Virtuoso/Neo4j-Zahlen (Results/*.xlsx, 60-s-Timeout, ms).
#
# Lädt den WDBench-Dump, dekomprimiert VOLLSTÄNDIG (bzip2, da lbzip2 diese Datei
# bei ~493M Zeilen abschneidet), baut unseren Snapshot, lädt den Server und misst
# alle fünf Query-Klassen (komplette Sets, kein Cap) mit wdbench_bench.py.
#
# Aufruf:  ./wdbench_solo.sh [stride] [timeout_s]
#   stride=1 (default) -> voller 1,257-Mrd.-Graph (für den echten Vergleich).
#   timeout_s default 60 (wie die Referenz).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Kanonischer, VOLLSTÄNDIGER WDBench-Dump (9,15 GB .nt.bz2 -> ~1,257 Mrd. Tripel).
# NICHT die truncated latest_truthy_data_filtered.tar.bz2 (gibt nur ~495M her).
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

# --- 1. Daten: laden (MD5) + dekomprimieren (reines .nt.bz2, kein tar) -------
if [ ! -s "${FULL}" ]; then
    if [ ! -s "${ARCHIVE}" ] || [ "$(md5of "${ARCHIVE}")" != "${EXPECTED_MD5}" ]; then
        log "Lade vollständigen WDBench-Dump (9,15 GB)..."
        rm -f "${ARCHIVE}"
        curl -L --fail --retry 6 --retry-delay 5 -o "${ARCHIVE}" "${DATA_URL}"
        [ "$(md5of "${ARCHIVE}")" = "${EXPECTED_MD5}" ] && log "MD5 ok." || log "WARN: MD5 weicht ab."
    fi
    # bzip2 (single-threaded, ~40-60 min, aber verlässlich; auf dieser
    # vollständigen Datei verifiziert). Reines .nt.bz2 -> KEIN tar. Fail-loud:
    # ein Decompress-Fehler bricht ab, statt still eine Teildatei zu nutzen.
    log "Dekomprimiere (bzip2) -> ${FULL} ..."
    bzip2 -dc "${ARCHIVE}" > "${FULL}.partial"
    mv "${FULL}.partial" "${FULL}"
fi
FULL_LINES=$(wc -l < "${FULL}" | tr -d ' ')
log "Dekomprimiert: ${FULL_LINES} Zeilen"
if [ "${STRIDE}" = "1" ]; then NT="${FULL}"; else
    [ -s "${NT}" ] || awk "NR % ${STRIDE} == 1" "${FULL}" > "${NT}"
fi
TRIPLES=$(wc -l < "${NT}" | tr -d ' ')
log "Datensatz: ${TRIPLES} Tripel (stride=${STRIDE})"

# --- 2. Query-Logs -> .rq (KOMPLETTE Sets, kein Cap) ------------------------
for f in single_bgps multiple_bgps opts paths c2rpqs; do
    [ -f "${QSRC}/${f}.txt" ] || curl -sL -o "${QSRC}/${f}.txt" "${REPO_RAW}/${f}.txt"
done
python3 "${ROOT}/wdbench_queries.py" "${QSRC}" "${QDIR}"

# --- 3. Unsere Engine: build + mmap-load (gemessen) -------------------------
log "Baue Snapshot + lade Server..."
export PATH="${PATH}:/root/.cargo/bin:${HOME:-/root}/.cargo/bin"
[ -x "${ROOT}/target/release/server" ] || ( cd "${ROOT}" && cargo build --release --bin server >/dev/null 2>&1 )
B0=$(now_ms)
"${ROOT}/target/release/server" build "${NT}" "${SNAP}" >/dev/null
"${ROOT}/target/release/server" load "${SNAP}" "${PORT}" >/tmp/solo_srv.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
wait_port "${PORT}" || { log "Server nicht bereit"; cat /tmp/solo_srv.log; exit 1; }
INGEST_MS=$(( $(now_ms) - B0 ))

# --- 4. Benchmark je Klasse (publiziertes Format) ---------------------------
echo
echo "=========== Trillian WDBench (${TRIPLES} Tripel, ${TIMEOUT}s-Timeout) ==========="
RSSKB=$(awk '/VmRSS/{print $2}' "/proc/${SRV}/status" 2>/dev/null || echo "")
echo "Ingest+Load: ${INGEST_MS} ms | Snapshot-Disk: $(du -m "${SNAP}" 2>/dev/null | cut -f1) MB | RSS: ${RSSKB:-n/a} KB"
echo "Label          Kategorie      Aggregat (ms)"
echo "-------------------------------------------------------------------------------"
for cat in single_bgps multiple_bgps opts paths c2rpqs; do
    python3 "${ROOT}/wdbench_bench.py" "http://localhost:${PORT}/sparql" "${QDIR}/${cat}" \
        --timeout "${TIMEOUT}" --label trillian --csv "${DATADIR}/solo_${cat}.csv" || true
done
log "Fertig. Per-Query-CSVs in ${DATADIR}/solo_*.csv"
