#!/usr/bin/env bash
# run_aws_bench.sh
#
# Provisions an EC2 instance, deploys our engine, and runs the solo WDBench
# benchmark (benchmarks/wdbench_solo.sh) for the absolute comparison with the published
# Blazegraph/Jena/Virtuoso/Neo4j numbers.
#
# Local prerequisites: terraform, ansible, AWS credentials.
# Generates a dedicated SSH key pair under infra/aws/terraform/duel_key.
#
# Usage:  ./run_aws_bench.sh [stride] [timeout_s]
#   stride=1 (default) -> full 1.257-billion-triple graph. timeout_s default 60.
#   Box overridable via env (default r6i.16xlarge/400 GB):
#     INSTANCE_TYPE=r6i.4xlarge DISK_GB=120 ./run_aws_bench.sh 10 60
#
# COST: the instance runs until ./destroy.sh. r6i.16xlarge ~$4/h; only one engine
#   resident, hence moderate RAM needs.
set -euo pipefail

STRIDE="${1:-1}"
TIMEOUT="${2:-60}"
# Default box: only our engine is resident.
: "${INSTANCE_TYPE:=r6i.16xlarge}"
: "${DISK_GB:=400}"
export INSTANCE_TYPE DISK_GB

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TERRAFORM_DIR="${SCRIPT_DIR}/terraform"
ANSIBLE_DIR="${SCRIPT_DIR}/ansible"
SSH_PRIVATE_KEY="${TERRAFORM_DIR}/duel_key"
SSH_PUBLIC_KEY="${TERRAFORM_DIR}/duel_key.pub"

# Roughly check AWS credentials.
if ! aws sts get-caller-identity >/dev/null 2>&1; then
    echo "ERROR: No valid AWS credentials (aws configure / AWS_PROFILE / keys)."
    exit 1
fi

if [ ! -f "$SSH_PRIVATE_KEY" ]; then
    echo "[run_aws_bench] Generating a dedicated SSH key pair..."
    ssh-keygen -t ed25519 -f "$SSH_PRIVATE_KEY" -N "" -C "trillian-wdbench"
fi

echo "[run_aws_bench] Terraform apply..."
cd "$TERRAFORM_DIR"
terraform init -input=false
# Optional per-run overrides (e.g. a cheaper warm-up):
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

echo "[run_aws_bench] Waiting for SSH..."
MAX_WAIT=180; ELAPSED=0
while ! ssh -i "$SSH_PRIVATE_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
            -o ConnectTimeout=5 -q ubuntu@"$SERVER_IP" exit 2>/dev/null; do
    if [ "$ELAPSED" -ge "$MAX_WAIT" ]; then echo "ERROR: SSH not reachable."; exit 1; fi
    sleep 5; ELAPSED=$((ELAPSED + 5))
    echo "[run_aws_bench] ... (${ELAPSED}s/${MAX_WAIT}s)"
done
echo "[run_aws_bench] SSH ready."

cat > "${ANSIBLE_DIR}/inventory.ini" <<EOF
[wdbench]
$SERVER_IP ansible_user=ubuntu ansible_ssh_private_key_file=$SSH_PRIVATE_KEY ansible_ssh_extra_args='-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null'
EOF

echo "[run_aws_bench] Ansible deployment (build our engine + solo benchmark, may take hours)..."
cd "$ANSIBLE_DIR"
ANSIBLE_HOST_KEY_CHECKING=False ansible-playbook -i inventory.ini playbook_solo.yml \
    -e "wdbench_stride=${STRIDE} wdbench_timeout=${TIMEOUT}"

echo "[run_aws_bench] Fetching result..."
scp -i "$SSH_PRIVATE_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "ubuntu@$SERVER_IP:/opt/trillian/wdbench_solo.log" \
    "${SCRIPT_DIR}/wdbench_solo.log" 2>/dev/null || echo "  (no log found)"
# Per-query CSVs for the correctness comparison (benchmarks/wdbench_compare.py).
mkdir -p "${SCRIPT_DIR}/solo_csv"
scp -i "$SSH_PRIVATE_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "ubuntu@$SERVER_IP:/opt/trillian/wdbench_data/solo_*.csv" \
    "${SCRIPT_DIR}/solo_csv/" 2>/dev/null || echo "  (no CSVs found)"

echo "[run_aws_bench] Done. Result: ${SCRIPT_DIR}/wdbench_solo.log + solo_csv/"
echo ""
echo "IMPORTANT – the instance keeps costing money until you destroy it:"
echo "  ./infra/aws/destroy.sh"
