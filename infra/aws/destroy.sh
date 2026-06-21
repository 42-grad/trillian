#!/usr/bin/env bash
# Zerstört die AWS-WDBench-Instanz und alle zugehörigen Ressourcen.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TERRAFORM_DIR="${SCRIPT_DIR}/terraform"
SSH_PUBLIC_KEY="${TERRAFORM_DIR}/duel_key.pub"

if ! aws sts get-caller-identity >/dev/null 2>&1; then
    echo "FEHLER: Keine gültigen AWS-Credentials."
    exit 1
fi

cd "$TERRAFORM_DIR"
terraform destroy -auto-approve -var="ssh_public_key_path=$SSH_PUBLIC_KEY"
echo "[destroy] AWS-Ressourcen entfernt."
