# AWS-Konstrukt für den vollen WDBench-Duell

Eigenständiges Terraform/Ansible-Setup (parallel zum Hetzner-Setup unter
`infra/`), um den **vollen WDBench-Vergleich** (Wikidata Truthy, 1,26 Mrd.
Tripel) Rust-Clone vs. C++ Tentris auf einer Big-RAM-EC2-Instanz zu fahren.

## Warum AWS / Big-RAM
Beide Engines laufen beim Duell **gleichzeitig** resident:
- Rust-Clone ~130 GB logisch (Index davon mmap-pageable),
- Tentris ~400 GB (≈340 B/Triple),
- zusammen ~530 GB RAM → Default **r6i.24xlarge** (96 vCPU / 768 GB).

Disk: dekomprimierte `.nt` (~100 GB) + Rust-Snapshot (~50 GB) + Tentris-metall
(~1,3 TB) → Default **2 TB gp3**.

## Voraussetzungen
- `terraform`, `ansible` lokal
- AWS-Credentials (`aws configure`, `AWS_PROFILE` oder `AWS_ACCESS_KEY_ID`/`SECRET`)

## vCPU-Quota beachten
`r6i.24xlarge` = 96 vCPU. Neue Accounts haben oft nur 32–64 vCPU
("Running On-Demand Standard instances", Quota-Code `L-1216C47A`). Prüfen:
```bash
aws service-quotas get-service-quota --service-code ec2 \
  --quota-code L-1216C47A --region us-east-1 --query 'Quota.Value'
```
Reicht es nicht, entweder Erhöhung beantragen
(`request-service-quota-increase ... --desired-value 96`, Freigabe dauert) oder
eine kleinere Instanz per Env-Override fahren (siehe unten). Größte Instanz bei
64 vCPU: `r6i.16xlarge` (512 GB).

## Start
```bash
# Voller Datensatz, 100 Queries je Kategorie (braucht 96-vCPU-Quota):
./infra/aws/run_aws_duel.sh

# Günstiger Warm-up auf kleiner Instanz (Env-Override für Typ/Disk):
INSTANCE_TYPE=r6i.4xlarge DISK_GB=300 ./infra/aws/run_aws_duel.sh 10 50
```
`INSTANCE_TYPE` / `DISK_GB` überschreiben die Terraform-Defaults pro Lauf.
Das Skript erzeugt ein dediziertes SSH-Keypair (`terraform/duel_key`, **nie**
committen), provisioniert die Instanz, baut beide Engines, lädt den
WDBench-Dump (Figshare, 3,6 GB → entpackt), fährt `wdbench_duel.sh` und holt
`wdbench_duel.log` zurück.

## Aufräumen (WICHTIG – Instanz kostet weiter)
```bash
./infra/aws/destroy.sh
```

## Kosten / Knöpfe
- `instance_type` (variables.tf): r6i.24xlarge (768 GB, ~6 $/h) | r6i.16xlarge
  (512 GB; dann mit `stride` fahren) | x2iedn.16xlarge (1 TB).
- `disk_gb`: bei `stride` deutlich kleiner wählbar.
- `aws_region`, `ssh_ingress_cidrs` (für SSH-Whitelisting eigene IP/32 setzen).
- Vor dem teuren Lauf lohnt die lokale/kleine Machbarkeits-Probe:
  `bash wdbench_probe.sh`.
