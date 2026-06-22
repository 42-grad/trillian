#!/usr/bin/env bash
# run_aws_bench.sh
#
# Provisioniert eine EC2-Instanz, deployt NUR unsere Engine (kein Tentris) und
# fährt den Solo-WDBench-Benchmark (wdbench_solo.sh) für den absoluten Vergleich
# mit den publizierten Blazegraph/Jena/Virtuoso/Neo4j-Zahlen.
#
# Voraussetzungen lokal: terraform, ansible, AWS-Credentials.
# Erzeugt ein dediziertes SSH-Key-Paar unter infra/aws/terraform/duel_key.
#
# Aufruf:  ./run_aws_bench.sh [stride] [timeout_s]
#   stride=1 (default) -> voller 1,257-Mrd.-Graph. timeout_s default 60.
#   Box per Env überschreibbar (Default r6i.16xlarge/400 GB):
#     INSTANCE_TYPE=r6i.4xlarge DISK_GB=120 ./run_aws_bench.sh 10 60
#
# KOSTEN: Instanz läuft bis ./destroy.sh. r6i.16xlarge ~4 $/h; ohne Tentris
#   muss nur eine Engine resident sein -> kleiner/günstiger als der Duell-Lauf.
set -euo pipefail

STRIDE="${1:-1}"
TIMEOUT="${2:-60}"
# Default kleinere Box: ohne Tentris muss nur UNSERE Engine resident sein.
: "${INSTANCE_TYPE:=r6i.16xlarge}"
: "${DISK_GB:=400}"
export INSTANCE_TYPE DISK_GB

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
    echo "[run_aws_bench] Erzeuge dediziertes SSH-Key-Paar..."
    ssh-keygen -t ed25519 -f "$SSH_PRIVATE_KEY" -N "" -C "tentris-wdbench"
fi

echo "[run_aws_bench] Terraform apply..."
cd "$TERRAFORM_DIR"
terraform init -input=false
# Optionale Overrides pro Lauf (z. B. günstiger Warm-up):
#   INSTANCE_TYPE=r6i.4xlarge DISK_GB=300 ./run_aws_bench.sh 10 50
EXTRA_VARS=()
[ -n "${INSTANCE_TYPE:-}" ] && EXTRA_VARS+=(-var="instance_type=${INSTANCE_TYPE}")
[ -n "${DISK_GB:-}" ] && EXTRA_VARS+=(-var="disk_gb=${DISK_GB}")
terraform apply -auto-approve \
    -var="ssh_public_key_path=$SSH_PUBLIC_KEY" \
    "${EXTRA_VARS[@]+"${EXTRA_VARS[@]}"}"

SERVER_IP=$(terraform output -raw server_ip)
LAUNCHED_TYPE=$(terraform output -raw instance_type)
echo "[run_aws_bench] Server: $SERVER_IP ($LAUNCHED_TYPE)"

echo "[run_aws_bench] Warte auf SSH..."
MAX_WAIT=180; ELAPSED=0
while ! ssh -i "$SSH_PRIVATE_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
            -o ConnectTimeout=5 -q ubuntu@"$SERVER_IP" exit 2>/dev/null; do
    if [ "$ELAPSED" -ge "$MAX_WAIT" ]; then echo "FEHLER: SSH nicht erreichbar."; exit 1; fi
    sleep 5; ELAPSED=$((ELAPSED + 5))
    echo "[run_aws_bench] ... (${ELAPSED}s/${MAX_WAIT}s)"
done
echo "[run_aws_bench] SSH bereit."

cat > "${ANSIBLE_DIR}/inventory.ini" <<EOF
[wdbench]
$SERVER_IP ansible_user=ubuntu ansible_ssh_private_key_file=$SSH_PRIVATE_KEY ansible_ssh_extra_args='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null'
EOF

echo "[run_aws_bench] Ansible-Deployment (Build unserer Engine + Solo-Benchmark, kann Stunden dauern)..."
cd "$ANSIBLE_DIR"
ANSIBLE_HOST_KEY_CHECKING=False ansible-playbook -i inventory.ini playbook_solo.yml \
    -e "wdbench_stride=${STRIDE} wdbench_timeout=${TIMEOUT}"

echo "[run_aws_bench] Hole Ergebnis..."
scp -i "$SSH_PRIVATE_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "ubuntu@$SERVER_IP:/opt/trillian/wdbench_solo.log" \
    "${SCRIPT_DIR}/wdbench_solo.log" 2>/dev/null || echo "  (kein Log gefunden)"
# Per-Query-CSVs für den Korrektheits-Vergleich (wdbench_compare.py).
mkdir -p "${SCRIPT_DIR}/solo_csv"
scp -i "$SSH_PRIVATE_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "ubuntu@$SERVER_IP:/opt/trillian/wdbench_data/solo_*.csv" \
    "${SCRIPT_DIR}/solo_csv/" 2>/dev/null || echo "  (keine CSVs gefunden)"

echo "[run_aws_bench] Fertig. Ergebnis: ${SCRIPT_DIR}/wdbench_solo.log + solo_csv/"
echo ""
echo "WICHTIG – Instanz kostet weiter, bis du sie zerstörst:"
echo "  ./infra/aws/destroy.sh"
