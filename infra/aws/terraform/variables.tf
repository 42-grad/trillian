variable "aws_region" {
  description = "AWS region"
  type        = string
  default     = "us-east-1"
}

variable "ssh_public_key_path" {
  description = "Path to the SSH public key installed on the host"
  type        = string
  default     = "infra/aws/terraform/duel_key.pub"
}

variable "instance_type" {
  description = <<-EOT
    EC2 instance type. Trillian holds the full 1.26B-triple graph resident in
    ~15-44 GB RAM, with headroom for the in-RAM build peak. r6i.16xlarge
    (64 vCPU / 512 GB) is comfortable; r6i.4xlarge (128 GB) is fine for a
    smaller --stride run. Overridable via the INSTANCE_TYPE env var.
  EOT
  type        = string
  default     = "r6i.16xlarge"
}

variable "disk_gb" {
  description = <<-EOT
    Root EBS in GB. Full run: decompressed .nt (~156 GB) + the snapshot (~49 GB)
    -> 400 GB default. Use less for a smaller --stride run. Overridable via DISK_GB.
  EOT
  type        = number
  default     = 400
}

variable "ssh_ingress_cidrs" {
  description = "Allowed source CIDRs for SSH (set your own IP/32 to lock it down)"
  type        = list(string)
  default     = ["0.0.0.0/0"]
}
