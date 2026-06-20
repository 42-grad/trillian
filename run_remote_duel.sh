#!/usr/bin/env bash
# run_remote_duel.sh
#
# Provisioniert einen Hetzner-Cloud-Server, deployt Rust-Klon + Tentris,
# baut beide Systeme und führt final_duel.py aus.
#
# Voraussetzungen lokal:
#   - terraform
#   - ansible
#   - HCLOUD_TOKEN als Umgebungsvariable
#
# Das Skript erzeugt selbst ein dediziertes SSH-Key-Paar unter
# infra/terraform/duel_key, sodass keine bestehenden Keys genutzt werden.
#
# Kosten: Der Server läuft, bis du ihn zerstörst. Siehe infra/terraform/destroy.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INFRA_DIR="${SCRIPT_DIR}/infra"
TERRAFORM_DIR="${INFRA_DIR}/terraform"
ANSIBLE_DIR="${INFRA_DIR}/ansible"

HCLOUD_TOKEN="${HCLOUD_TOKEN:-}"
SSH_PRIVATE_KEY="${TERRAFORM_DIR}/duel_key"
SSH_PUBLIC_KEY="${TERRAFORM_DIR}/duel_key.pub"

if [ -z "$HCLOUD_TOKEN" ]; then
    echo "FEHLER: HCLOUD_TOKEN ist nicht gesetzt."
    echo "Exportiere ihn vor dem Start: export HCLOUD_TOKEN=..."
    exit 1
fi

# Dediziertes SSH-Key-Paar erzeugen, falls nicht vorhanden
if [ ! -f "$SSH_PRIVATE_KEY" ]; then
    echo "[run_remote_duel] Erzeuge dediziertes SSH-Key-Paar..."
    ssh-keygen -t ed25519 -f "$SSH_PRIVATE_KEY" -N "" -C "tentris-duel"
fi

# Sanity-Check: beide Key-Dateien müssen existieren
if [ ! -f "$SSH_PUBLIC_KEY" ]; then
    echo "FEHLER: SSH public key nicht gefunden: $SSH_PUBLIC_KEY"
    exit 1
fi

echo "[run_remote_duel] Starte Terraform..."
cd "$TERRAFORM_DIR"
terraform init
terraform apply -auto-approve \
    -var="hcloud_token=$HCLOUD_TOKEN" \
    -var="ssh_public_key_path=$SSH_PUBLIC_KEY"

SERVER_IP=$(terraform output -raw server_ip)
echo "[run_remote_duel] Server IP: $SERVER_IP"

# Warte, bis SSH erreichbar ist (mit Timeout)
echo "[run_remote_duel] Warte auf SSH-Dienst..."
MAX_WAIT=120
ELAPSED=0
while ! ssh -i "$SSH_PRIVATE_KEY" \
            -o StrictHostKeyChecking=no \
            -o UserKnownHostsFile=/dev/null \
            -o ConnectTimeout=5 \
            -q root@"$SERVER_IP" exit 2>/dev/null; do
    if [ "$ELAPSED" -ge "$MAX_WAIT" ]; then
        echo "FEHLER: SSH nicht innerhalb von ${MAX_WAIT}s erreichbar."
        exit 1
    fi
    echo "[run_remote_duel] SSH noch nicht bereit, warte... (${ELAPSED}s/${MAX_WAIT}s)"
    sleep 5
    ELAPSED=$((ELAPSED + 5))
done
echo "[run_remote_duel] SSH ist bereit."

# Ansible Inventory dynamisch erzeugen
cat > "${ANSIBLE_DIR}/inventory.ini" <<EOF
[duel]
$SERVER_IP ansible_user=root ansible_ssh_private_key_file=$SSH_PRIVATE_KEY ansible_ssh_extra_args='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null'
EOF

echo "[run_remote_duel] Starte Ansible-Deployment (das kann 20-40 Minuten dauern)..."
cd "$ANSIBLE_DIR"
ANSIBLE_HOST_KEY_CHECKING=False ansible-playbook -i inventory.ini playbook.yml

echo "[run_remote_duel] Hole Ergebnisse vom Server..."
scp -i "$SSH_PRIVATE_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "root@$SERVER_IP:/opt/tentris_clone/duel_output.log" \
    "${SCRIPT_DIR}/duel_output.log"

echo "[run_remote_duel] Fertig! Ergebnisse in: ${SCRIPT_DIR}/duel_output.log"
echo ""
echo "Zum Aufräumen (Server löschen):"
echo "  ./infra/terraform/destroy.sh"
