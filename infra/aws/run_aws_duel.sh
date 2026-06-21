#!/usr/bin/env bash
# run_aws_duel.sh
#
# Provisioniert eine Big-RAM-EC2-Instanz, deployt Rust-Clone + Tentris, baut
# beide und führt den vollen WDBench-Duell (1,26 Mrd. Tripel) aus.
#
# Voraussetzungen lokal:
#   - terraform, ansible
#   - AWS-Credentials (z. B. `aws configure` / AWS_PROFILE / AWS_ACCESS_KEY_ID + SECRET)
#
# Erzeugt selbst ein dediziertes SSH-Key-Paar unter infra/aws/terraform/duel_key.
#
# Aufruf:  ./run_aws_duel.sh [stride] [max_per_category]
#   stride=1 (default) -> voller Datensatz. stride=N -> jede N-te Zeile.
#   max_per_category default 100.
#
# KOSTEN: Die Instanz läuft, bis du sie zerstörst (./destroy.sh). r6i.24xlarge
#   liegt on-demand bei ~6 $/h; ein voller Lauf kann mehrere Stunden dauern.
set -euo pipefail

STRIDE="${1:-1}"
MAXQ="${2:-100}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TERRAFORM_DIR="${SCRIPT_DIR}/terraform"
ANSIBLE_DIR="${SCRIPT_DIR}/ansible"
SSH_PRIVATE_KEY="${TERRAFORM_DIR}/duel_key"
SSH_PUBLIC_KEY="${TERRAFORM_DIR}/duel_key.pub"

# AWS-Credentials grob prüfen.
if ! aws sts get-caller-identity >/dev/null 2>&1; then
    echo "FEHLER: Keine gültigen AWS-Credentials (aws configure / AWS_PROFILE / Keys)."
    exit 1
fi

if [ ! -f "$SSH_PRIVATE_KEY" ]; then
    echo "[run_aws_duel] Erzeuge dediziertes SSH-Key-Paar..."
    ssh-keygen -t ed25519 -f "$SSH_PRIVATE_KEY" -N "" -C "tentris-wdbench"
fi

echo "[run_aws_duel] Terraform apply..."
cd "$TERRAFORM_DIR"
terraform init -input=false
# Optionale Overrides pro Lauf (z. B. günstiger Warm-up):
#   INSTANCE_TYPE=r6i.4xlarge DISK_GB=300 ./run_aws_duel.sh 10 50
EXTRA_VARS=()
[ -n "${INSTANCE_TYPE:-}" ] && EXTRA_VARS+=(-var="instance_type=${INSTANCE_TYPE}")
[ -n "${DISK_GB:-}" ] && EXTRA_VARS+=(-var="disk_gb=${DISK_GB}")
terraform apply -auto-approve \
    -var="ssh_public_key_path=$SSH_PUBLIC_KEY" \
    "${EXTRA_VARS[@]+"${EXTRA_VARS[@]}"}"

SERVER_IP=$(terraform output -raw server_ip)
LAUNCHED_TYPE=$(terraform output -raw instance_type)
echo "[run_aws_duel] Server: $SERVER_IP ($LAUNCHED_TYPE)"

echo "[run_aws_duel] Warte auf SSH..."
MAX_WAIT=180; ELAPSED=0
while ! ssh -i "$SSH_PRIVATE_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
            -o ConnectTimeout=5 -q ubuntu@"$SERVER_IP" exit 2>/dev/null; do
    if [ "$ELAPSED" -ge "$MAX_WAIT" ]; then echo "FEHLER: SSH nicht erreichbar."; exit 1; fi
    sleep 5; ELAPSED=$((ELAPSED + 5))
    echo "[run_aws_duel] ... (${ELAPSED}s/${MAX_WAIT}s)"
done
echo "[run_aws_duel] SSH bereit."

cat > "${ANSIBLE_DIR}/inventory.ini" <<EOF
[wdbench]
$SERVER_IP ansible_user=ubuntu ansible_ssh_private_key_file=$SSH_PRIVATE_KEY ansible_ssh_extra_args='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null'
EOF

echo "[run_aws_duel] Ansible-Deployment (Build beider Engines + WDBench-Lauf, kann Stunden dauern)..."
cd "$ANSIBLE_DIR"
ANSIBLE_HOST_KEY_CHECKING=False ansible-playbook -i inventory.ini playbook.yml \
    -e "wdbench_stride=${STRIDE} wdbench_maxq=${MAXQ}"

echo "[run_aws_duel] Hole Ergebnis..."
scp -i "$SSH_PRIVATE_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "ubuntu@$SERVER_IP:/opt/trillian/wdbench_duel.log" \
    "${SCRIPT_DIR}/wdbench_duel.log" 2>/dev/null || echo "  (kein Log gefunden)"

echo "[run_aws_duel] Fertig. Ergebnis: ${SCRIPT_DIR}/wdbench_duel.log"
echo ""
echo "WICHTIG – Instanz kostet weiter, bis du sie zerstörst:"
echo "  ./infra/aws/destroy.sh"
