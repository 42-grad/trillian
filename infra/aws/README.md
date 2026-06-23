# AWS construct for the full-scale WDBench benchmark

A self-contained Terraform + Ansible setup to benchmark Trillian on the full
**WDBench** dataset (Wikidata Truthy, 1,257,169,959 triples) on a single EC2
instance, and compare the results against the published WDBench numbers.

## Why a big-RAM box

Trillian holds the whole graph resident (~44 GB RAM at full scale, snapshot
~49 GB on disk). The default instance has comfortable headroom for the in-RAM
build peak and the decompressed ~156 GB `.nt`, so the disk is sized at 2 TB.

## Prerequisites

- `terraform`, `ansible` locally
- AWS credentials (`aws configure`, `AWS_PROFILE`, or access keys)

## Run

```bash
# full dataset, 60 s per-query timeout (default):
./infra/aws/run_aws_bench.sh

# smaller real warm-up (every 10th triple, on a cheaper box):
INSTANCE_TYPE=r6i.4xlarge DISK_GB=300 ./infra/aws/run_aws_bench.sh 10 60
```

The script provisions the instance, builds the Rust engine, downloads the
WDBench dump, builds a snapshot, runs the five query classes (`wdbench_solo.sh`),
and fetches `wdbench_solo.log` plus per-query CSVs back. It creates a dedicated
SSH keypair under `terraform/duel_key` (never committed).

## Tear down (important — the instance bills until destroyed)

```bash
./infra/aws/destroy.sh
```

## Knobs

- `INSTANCE_TYPE` / `DISK_GB` — env overrides per run.
- `aws_region`, `ssh_ingress_cidrs` (set your own IP/32 to lock down SSH).
- Results are compared against the published numbers in `wdbench_reference.md`
  via `wdbench_compare.py`.
