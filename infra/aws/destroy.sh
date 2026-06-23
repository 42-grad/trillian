#!/usr/bin/env bash
# Destroys the AWS WDBench instance and all associated resources.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TERRAFORM_DIR="${SCRIPT_DIR}/terraform"
SSH_PUBLIC_KEY="${TERRAFORM_DIR}/duel_key.pub"

if ! aws sts get-caller-identity >/dev/null 2>&1; then
    echo "ERROR: No valid AWS credentials."
    exit 1
fi

cd "$TERRAFORM_DIR"
terraform destroy -auto-approve -var="ssh_public_key_path=$SSH_PUBLIC_KEY"
echo "[destroy] AWS resources removed."
