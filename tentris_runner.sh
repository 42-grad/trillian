#!/usr/bin/env bash
# tentris_runner.sh
# Lädt das passende kommerzielle Tentris-Binary aus dem GitHub-Release herunter.
# Plattformen:
#   - Linux x86_64  -> remote / CI
#   - Darwin arm64  -> Apple Silicon (lokal)
#
# Das Binary wird nach third_party/tentris/tentris entpackt.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TENTRIS_DIR="${SCRIPT_DIR}/third_party/tentris"
NT_FILE="${SCRIPT_DIR}/synthetic_1m.nt"
RELEASE_VERSION="v0.22.5-beta"

log() {
    echo "[tentris_runner] $*"
}

detect_asset() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "${os}-${arch}" in
        Linux-x86_64)
            echo "tentris-${RELEASE_VERSION}-x86_64-linux.tar.gz"
            ;;
        Darwin-arm64|Darwin-aarch64)
            echo "tentris-${RELEASE_VERSION}-aarch64-darwin.tar.gz"
            ;;
        *)
            log "FEHLER: Nicht unterstützte Plattform ${os}-${arch}."
            log "Verfügbare Assets siehe: https://github.com/tentris/tentris/releases"
            exit 1
            ;;
    esac
}

download() {
    local asset="$1"
    local url="https://github.com/tentris/tentris/releases/download/v0.22.5/${asset}"
    local tmp_archive="/tmp/${asset}"

    mkdir -p "${TENTRIS_DIR}"

    if [ -f "${TENTRIS_DIR}/tentris" ]; then
        log "Tentris-Binary existiert bereits unter ${TENTRIS_DIR}/tentris."
        return
    fi

    log "Lade ${asset} herunter..."
    curl -sL -o "${tmp_archive}" "${url}"

    log "Entpacke nach ${TENTRIS_DIR}..."
    tar -xzf "${tmp_archive}" -C "${TENTRIS_DIR}"
    chmod +x "${TENTRIS_DIR}/tentris"

    log "Download abgeschlossen: ${TENTRIS_DIR}/tentris"
}

usage() {
    echo "Usage: $0 {download|status|ingest}"
    echo ""
    echo "  download  Tentris-Binary für die aktuelle Plattform herunterladen"
    echo "  status    Zeigt gefundene Dateien"
    echo "  ingest    Zeigt Hinweis zum Ingest-Befehl"
}

case "${1:-download}" in
    download|build)
        download "$(detect_asset)"
        ;;

    status)
        if [ -x "${TENTRIS_DIR}/tentris" ]; then
            log "Tentris-Binary: ${TENTRIS_DIR}/tentris"
            ls -la "${TENTRIS_DIR}/tentris"
        else
            log "Kein Tentris-Binary gefunden. Führe '$0 download' aus."
        fi
        ;;

    ingest)
        if [ ! -f "${NT_FILE}" ]; then
            log "FEHLER: ${NT_FILE} nicht gefunden."
            log "Bitte zuerst 'cargo run --release' ausführen, um die .nt-Datei zu erzeugen."
            exit 1
        fi
        log "Ingest-Befehl: ${TENTRIS_DIR}/tentris load --force-no-snapshot < ${NT_FILE}"
        ;;

    *)
        usage
        exit 1
        ;;
esac
