#!/usr/bin/env bash
# Zerstört den Hetzner-Server und alle dazugehörigen Ressourcen.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HCLOUD_TOKEN="${HCLOUD_TOKEN:-}"
SSH_PUBLIC_KEY="${SCRIPT_DIR}/duel_key.pub"

if [ -z "$HCLOUD_TOKEN" ]; then
    echo "FEHLER: HCLOUD_TOKEN ist nicht gesetzt."
    exit 1
fi

# Fallback auf existierenden Public Key, falls dedizierter Key noch nicht erzeugt wurde
if [ ! -f "$SSH_PUBLIC_KEY" ]; then
    SSH_PUBLIC_KEY="${HOME}/.ssh/id_rsa.pub"
fi

cd "$SCRIPT_DIR"
terraform destroy -auto-approve \
    -var="hcloud_token=$HCLOUD_TOKEN" \
    -var="ssh_public_key_path=$SSH_PUBLIC_KEY"
