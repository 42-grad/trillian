variable "aws_region" {
  description = "AWS-Region"
  type        = string
  default     = "us-east-1"
}

variable "ssh_public_key_path" {
  description = "Pfad zum SSH-Public-Key, der auf dem Host hinterlegt wird"
  type        = string
  default     = "infra/aws/terraform/duel_key.pub"
}

variable "instance_type" {
  description = <<-EOT
    EC2-Instanztyp. Beide Engines laufen beim Duell GLEICHZEITIG resident
    (Rust ~130 GB logisch + Tentris ~400 GB ≈ 530 GB RAM für den vollen
    Datensatz). Default r6i.24xlarge = 96 vCPU / 768 GB (sicher für beide).
    Sparvarianten: r6i.16xlarge (512 GB; mit --stride fahren) oder für reine
    Probe r6i.4xlarge (128 GB, nur Rust). x2iedn.16xlarge = 1 TB.
  EOT
  type        = string
  default     = "r6i.24xlarge"
}

variable "disk_gb" {
  description = <<-EOT
    Root-EBS in GB. Voll: dekomprimierte .nt (~100 GB) + Rust-Snapshot (~50 GB)
    + Tentris-metall (~1,3 TB) -> 2000 GB Default. Für --stride entsprechend
    weniger.
  EOT
  type        = number
  default     = 2000
}

variable "ssh_ingress_cidrs" {
  description = "Erlaubte Quell-CIDRs für SSH (zur Absicherung ggf. eigene IP/32 setzen)"
  type        = list(string)
  default     = ["0.0.0.0/0"]
}
