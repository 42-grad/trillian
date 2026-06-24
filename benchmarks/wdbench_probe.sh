#!/usr/bin/env bash
# wdbench_probe.sh — feasibility/scaling probe for WDBench (Wikidata Truthy,
# 1,257,169,959 triples) BEFORE booking a big-RAM box.
#
# On REAL WDBench data, measures the memory footprint of our engine at growing
# triple counts and projects to the full dataset. Also downloads the WDBench
# query logs, converts them, and runs a sample (incl. property paths + C2RPQs)
# against our server — end-to-end validation of the features on real Wikidata
# shapes, without a big-RAM box.
#
# Usage:  ./wdbench_probe.sh ["N1 N2 N3"]   # slice sizes (lines), default below
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"   # benchmarks/
ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"                       # project root
DATA_URL="https://ndownloader.figshare.com/files/34816081"   # truthy_direct_properties.nt.bz2 (9.15 GB, complete)
REPO_RAW="https://raw.githubusercontent.com/MillenniumDB/WDBench/master/Queries"
TOTAL_TRIPLES=1257169959

SIZES="${1:-2000000 8000000 32000000}"
MAXN=$(echo "$SIZES" | tr ' ' '\n' | sort -n | tail -1)

DATADIR="${ROOT}/wdbench_data"
QSRC="${ROOT}/wdbench_qsrc"
QDIR="${ROOT}/wdbench_queries"
PORT=9091
mkdir -p "${DATADIR}" "${QSRC}"

log() { echo "[probe] $*"; }
now_ms() { date +%s%3N; }
wait_port() { for _ in $(seq 1 120); do (echo > "/dev/tcp/localhost/$1") 2>/dev/null && return 0; sleep 0.5; done; return 1; }

# --- 0. Build ---------------------------------------------------------------
log "Building the Rust server (release)..."
( cd "${ROOT}" && cargo build --release --bin server >/dev/null 2>&1 )
SERVER="${ROOT}/target/release/server"
printf 'SELECT ?x WHERE { ?s ?p ?x } LIMIT 1\n' > "${DATADIR}/dummy.rq"

# --- 1. Fetch + convert the WDBench query logs ------------------------------
log "Downloading the WDBench query logs..."
for f in single_bgps multiple_bgps opts paths c2rpqs; do
    [ -f "${QSRC}/${f}.txt" ] || curl -sL -o "${QSRC}/${f}.txt" "${REPO_RAW}/${f}.txt"
done
python3 "${SCRIPT_DIR}/wdbench_queries.py" "${QSRC}" "${QDIR}" 25   # sample: 25/category

# --- 2. Stream the largest slice ONCE (bzip2 stops early via SIGPIPE) --------
BIG="${DATADIR}/wdbench_${MAXN}.nt"
if [ ! -f "${BIG}" ]; then
    log "Streaming the largest slice (${MAXN} lines) from the 3.6-GB dump..."
    curl -sL "${DATA_URL}" | bzip2 -dc | head -n "${MAXN}" > "${BIG}" || true
fi
log "Largest slice: $(wc -l < "${BIG}") triples"

# --- 3. Measure footprint per slice -----------------------------------------
echo
echo "### Footprint scaling (engine, real WDBench data)"
printf "%12s  %10s  %9s  %8s  %8s  %9s  %10s\n" \
    "Triples" "Ingest" "Perm" "Dict" "PredLst" "Total" "B/Triple"
echo "----------------------------------------------------------------------------------"
LAST_BPT=""
for N in ${SIZES}; do
    SLICE="${DATADIR}/wdbench_${N}.nt"
    if [ ! -f "${SLICE}" ]; then head -n "${N}" "${BIG}" > "${SLICE}"; fi
    REAL_N=$(wc -l < "${SLICE}" | tr -d ' ')
    OUT=$("${SERVER}" profile "${SLICE}" "${DATADIR}/dummy.rq" 1 2>&1 || true)
    # Patterns track the English server output (memory_report / profile loader).
    ING=$(echo "$OUT"  | sed -n 's/.* triples, .* in \([0-9]*\) ms/\1/p' | head -1)
    PERM=$(echo "$OUT" | sed -n 's/.*permutations.*: *\([0-9.]*\) MB/\1/p' | head -1)
    DICT=$(echo "$OUT" | sed -n 's/.*Dictionary.*: *\([0-9.]*\) MB/\1/p' | head -1)
    PRED=$(echo "$OUT" | sed -n 's/.*Predicate subjects.*: *\([0-9.]*\) MB/\1/p' | head -1)
    SUM=$(echo "$OUT"  | sed -n 's/.*Total (logical): *\([0-9.]*\) MB/\1/p' | head -1)
    BPT=$(echo "$OUT"  | sed -n 's/.*Bytes\/triple (logical): *\([0-9]*\) B/\1/p' | head -1)
    LAST_BPT="${BPT:-$LAST_BPT}"
    printf "%12s  %8s ms  %7s M  %6s M  %6s M  %7s M  %8s B\n" \
        "${REAL_N}" "${ING:-?}" "${PERM:-?}" "${DICT:-?}" "${PRED:-?}" "${SUM:-?}" "${BPT:-?}"
done
echo "----------------------------------------------------------------------------------"

# --- 4. RSS of the real mmap server on the largest slice --------------------
log "Measuring RSS (mmap server) on the largest slice..."
SNAP="${DATADIR}/wdbench_${MAXN}.bin"
"${SERVER}" build "${BIG}" "${SNAP}" >/dev/null 2>&1
DISK_MB=$(du -m "${SNAP}" | cut -f1)
"${SERVER}" load "${SNAP}" "${PORT}" >/tmp/wdb_srv.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
wait_port "${PORT}" || { log "Server not ready"; cat /tmp/wdb_srv.log; exit 1; }
RSS_KB="n/a"; PEAK_KB="n/a"
if [ -r "/proc/${SRV}/status" ]; then
    RSS_KB=$(awk '/VmRSS:/{print $2}' "/proc/${SRV}/status")
    PEAK_KB=$(awk '/VmHWM:/{print $2}' "/proc/${SRV}/status")
fi

# --- 5. Feature sanity: real WDBench queries against our engine -------------
echo
echo "### Feature sanity (real WDBench queries against the engine, ${MAXN} slice)"
for cat in single_bgps multiple_bgps opts paths c2rpqs; do
    ok=0; err=0; rows_total=0; n=0
    for q in "${QDIR}/${cat}"/*.rq; do
        [ -f "$q" ] || continue
        n=$((n+1)); [ $n -gt 10 ] && break   # 10 per category
        BODY=$(python3 -c "import urllib.parse,sys; print(urllib.parse.quote(open(sys.argv[1]).read()))" "$q")
        RESP=$(curl -s -m 60 "http://localhost:${PORT}/sparql?query=${BODY}" -H "Accept: application/sparql-results+json" || echo "")
        if echo "$RESP" | grep -q '"bindings"'; then
            ok=$((ok+1))
            r=$(echo "$RESP" | python3 -c "import sys,json; print(len(json.load(sys.stdin)['results']['bindings']))" 2>/dev/null || echo 0)
            rows_total=$((rows_total+r))
        else
            err=$((err+1))
        fi
    done
    printf "  %-14s  executed %2d/%-2d  Σrows=%-8s %s\n" "$cat" "$ok" "$((ok+err))" "$rows_total" \
        "$([ $err -gt 0 ] && echo "(${err} errors)" || echo "OK")"
done

# --- 6. Projection ----------------------------------------------------------
echo
echo "### Projection to the full WDBench dataset (${TOTAL_TRIPLES} triples)"
RSS_MB="n/a"; PEAK_MB="n/a"; RSS_BPT="n/a"
if [ "$RSS_KB" != "n/a" ]; then
    RSS_MB=$(awk "BEGIN{printf \"%.0f\", ${RSS_KB}/1024}")
    PEAK_MB=$(awk "BEGIN{printf \"%.0f\", ${PEAK_KB}/1024}")
    RSS_BPT=$(awk "BEGIN{printf \"%.1f\", ${RSS_KB}*1024/$(wc -l < "${BIG}")}")
fi
echo "  Measured on the ${MAXN} slice:"
echo "    Snapshot disk:   ${DISK_MB} MB    (index zero-copy mmap-able)"
echo "    RSS / peak RSS:  ${RSS_MB} MB / ${PEAK_MB} MB"
echo "    Logical B/triple (largest slice): ${LAST_BPT:-?} B"
echo "    RSS     B/triple (largest slice): ${RSS_BPT} B"
if [ "${LAST_BPT:-}" != "" ]; then
    PROJ_LOG=$(awk "BEGIN{printf \"%.1f\", ${LAST_BPT}*${TOTAL_TRIPLES}/1024/1024/1024}")
    echo "  Projection @ ${TOTAL_TRIPLES} triples (logical B/triple held constant):"
    echo "    Engine logical:  ~${PROJ_LOG} GB"
    if [ "$RSS_BPT" != "n/a" ]; then
        PROJ_RSS=$(awk "BEGIN{printf \"%.1f\", ${RSS_BPT}*${TOTAL_TRIPLES}/1024/1024/1024}")
        echo "    Engine RSS:      ~${PROJ_RSS} GB"
    fi
    echo "  Note: B/triple drops with N (term reuse in the dict) -> the projection"
    echo "        above is an UPPER BOUND; the true full value is lower."
fi
echo
log "Done. Server PID ${SRV} is terminated on exit."
