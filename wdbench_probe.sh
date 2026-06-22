#!/usr/bin/env bash
# wdbench_probe.sh — Machbarkeits-/Skalierungs-Probe für WDBench (Wikidata
# Truthy, 1.257.169.959 Tripel) VOR dem Buchen einer Big-RAM-Box.
#
# Misst auf ECHTEN WDBench-Daten den Speicher-Footprint unseres Rust-Clones bei
# wachsender Tripelzahl und projiziert auf den Volldatensatz. Lädt zusätzlich
# die WDBench-Query-Logs, konvertiert sie und führt eine Stichprobe (inkl.
# Property-Paths + C2RPQs) gegen unseren Server aus — End-to-End-Validierung der
# neuen Features auf realen Wikidata-Shapes, ohne dass Tentris/Big-RAM nötig ist.
#
# Aufruf:  ./wdbench_probe.sh ["N1 N2 N3"]   # Slice-Größen (Zeilen), default unten
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA_URL="https://ndownloader.figshare.com/files/34816081"   # truthy_direct_properties.nt.bz2 (9,15 GB, vollständig)
REPO_RAW="https://raw.githubusercontent.com/MillenniumDB/WDBench/master/Queries"
TOTAL_TRIPLES=1257169959
TENTRIS_BPT=340.3   # gemessenes Tentris-RSS B/Triple (WatDiv-Lauf) als Referenz

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
log "Baue Rust-Server (release)..."
( cd "${ROOT}" && cargo build --release --bin server >/dev/null 2>&1 )
SERVER="${ROOT}/target/release/server"
printf 'SELECT ?x WHERE { ?s ?p ?x } LIMIT 1\n' > "${DATADIR}/dummy.rq"

# --- 1. WDBench-Query-Logs holen + konvertieren -----------------------------
log "Lade WDBench-Query-Logs..."
for f in single_bgps multiple_bgps opts paths c2rpqs; do
    [ -f "${QSRC}/${f}.txt" ] || curl -sL -o "${QSRC}/${f}.txt" "${REPO_RAW}/${f}.txt"
done
python3 "${ROOT}/wdbench_queries.py" "${QSRC}" "${QDIR}" 25   # Stichprobe: 25/Kategorie

# --- 2. Größten Slice EINMAL streamen (bzip2 stoppt dank SIGPIPE früh) -------
BIG="${DATADIR}/wdbench_${MAXN}.nt"
if [ ! -f "${BIG}" ]; then
    log "Streame größten Slice (${MAXN} Zeilen) aus dem 3.6-GB-Dump..."
    curl -sL "${DATA_URL}" | bzip2 -dc | head -n "${MAXN}" > "${BIG}" || true
fi
log "Größter Slice: $(wc -l < "${BIG}") Tripel"

# --- 3. Footprint je Slice messen -------------------------------------------
echo
echo "### Footprint-Skalierung (Rust-Clone, echte WDBench-Daten)"
printf "%12s  %10s  %9s  %8s  %8s  %9s  %10s\n" \
    "Triples" "Ingest" "Perm" "Dict" "PredLst" "Summe" "B/Triple"
echo "----------------------------------------------------------------------------------"
LAST_BPT=""
for N in ${SIZES}; do
    SLICE="${DATADIR}/wdbench_${N}.nt"
    if [ ! -f "${SLICE}" ]; then head -n "${N}" "${BIG}" > "${SLICE}"; fi
    REAL_N=$(wc -l < "${SLICE}" | tr -d ' ')
    OUT=$("${SERVER}" profile "${SLICE}" "${DATADIR}/dummy.rq" 1 2>&1 || true)
    ING=$(echo "$OUT"  | sed -n 's/.*Triples, .* in \([0-9]*\) ms/\1/p' | head -1)
    PERM=$(echo "$OUT" | sed -n 's/.*Permutationen.*: \([0-9.]*\) MB/\1/p' | head -1)
    DICT=$(echo "$OUT" | sed -n 's/.*Dictionary.*: *\([0-9.]*\) MB/\1/p' | head -1)
    PRED=$(echo "$OUT" | sed -n 's/.*Prädikat-Listen: *\([0-9.]*\) MB/\1/p' | head -1)
    SUM=$(echo "$OUT"  | sed -n 's/.*Summe (logisch): *\([0-9.]*\) MB/\1/p' | head -1)
    BPT=$(echo "$OUT"  | sed -n 's/.*Bytes\/Triple (logisch): *\([0-9]*\) B/\1/p' | head -1)
    LAST_BPT="${BPT:-$LAST_BPT}"
    printf "%12s  %8s ms  %7s M  %6s M  %6s M  %7s M  %8s B\n" \
        "${REAL_N}" "${ING:-?}" "${PERM:-?}" "${DICT:-?}" "${PRED:-?}" "${SUM:-?}" "${BPT:-?}"
done
echo "----------------------------------------------------------------------------------"

# --- 4. RSS des echten mmap-Servers auf dem größten Slice -------------------
log "Messe RSS (mmap-Server) auf größtem Slice..."
SNAP="${DATADIR}/wdbench_${MAXN}.bin"
"${SERVER}" build "${BIG}" "${SNAP}" >/dev/null 2>&1
DISK_MB=$(du -m "${SNAP}" | cut -f1)
"${SERVER}" load "${SNAP}" "${PORT}" >/tmp/wdb_srv.log 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
wait_port "${PORT}" || { log "Server nicht bereit"; cat /tmp/wdb_srv.log; exit 1; }
RSS_KB="n/a"; PEAK_KB="n/a"
if [ -r "/proc/${SRV}/status" ]; then
    RSS_KB=$(awk '/VmRSS:/{print $2}' "/proc/${SRV}/status")
    PEAK_KB=$(awk '/VmHWM:/{print $2}' "/proc/${SRV}/status")
fi

# --- 5. Feature-Sanity: echte WDBench-Queries gegen unsere Engine -----------
echo
echo "### Feature-Sanity (echte WDBench-Queries gegen Rust-Clone, ${MAXN}-Slice)"
for cat in single_bgps multiple_bgps opts paths c2rpqs; do
    ok=0; err=0; rows_total=0; n=0
    for q in "${QDIR}/${cat}"/*.rq; do
        [ -f "$q" ] || continue
        n=$((n+1)); [ $n -gt 10 ] && break   # 10 je Kategorie
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
    printf "  %-14s  ausgeführt %2d/%-2d  Σrows=%-8s %s\n" "$cat" "$ok" "$((ok+err))" "$rows_total" \
        "$([ $err -gt 0 ] && echo "(${err} Fehler)" || echo "OK")"
done

# --- 6. Projektion ----------------------------------------------------------
echo
echo "### Projektion auf den WDBench-Volldatensatz (${TOTAL_TRIPLES} Tripel)"
RSS_MB="n/a"; PEAK_MB="n/a"; RSS_BPT="n/a"
if [ "$RSS_KB" != "n/a" ]; then
    RSS_MB=$(awk "BEGIN{printf \"%.0f\", ${RSS_KB}/1024}")
    PEAK_MB=$(awk "BEGIN{printf \"%.0f\", ${PEAK_KB}/1024}")
    RSS_BPT=$(awk "BEGIN{printf \"%.1f\", ${RSS_KB}*1024/$(wc -l < "${BIG}")}")
fi
echo "  Gemessen am ${MAXN}-Slice:"
echo "    Snapshot-Disk:   ${DISK_MB} MB    (Index zero-copy mmap-bar)"
echo "    RSS / Peak-RSS:  ${RSS_MB} MB / ${PEAK_MB} MB"
echo "    Logisch B/Triple (größter Slice): ${LAST_BPT:-?} B"
echo "    RSS    B/Triple (größter Slice):  ${RSS_BPT} B"
if [ "${LAST_BPT:-}" != "" ]; then
    PROJ_LOG=$(awk "BEGIN{printf \"%.1f\", ${LAST_BPT}*${TOTAL_TRIPLES}/1024/1024/1024}")
    echo "  Projektion @ ${TOTAL_TRIPLES} Tripel (logisch B/Triple konstant gesetzt):"
    echo "    Rust logisch:  ~${PROJ_LOG} GB"
    if [ "$RSS_BPT" != "n/a" ]; then
        PROJ_RSS=$(awk "BEGIN{printf \"%.1f\", ${RSS_BPT}*${TOTAL_TRIPLES}/1024/1024/1024}")
        PROJ_TEN=$(awk "BEGIN{printf \"%.1f\", ${TENTRIS_BPT}*${TOTAL_TRIPLES}/1024/1024/1024}")
        echo "    Rust RSS:      ~${PROJ_RSS} GB   (Tentris-Referenz @${TENTRIS_BPT} B: ~${PROJ_TEN} GB)"
    fi
    echo "  Hinweis: B/Triple sinkt mit N (Term-Wiederverwendung im Dict) -> obige"
    echo "           Projektion ist eine OBERGRENZE; echter Vollwert liegt darunter."
fi
echo
log "Fertig. Server-PID ${SRV} wird beim Exit beendet."
