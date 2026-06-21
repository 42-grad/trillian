terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.40"
    }
  }
}

provider "aws" {
  region = var.aws_region
}

# Aktuelles Ubuntu-24.04-AMI (Canonical) in der gewählten Region.
data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"]
  }
  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

# Default-VPC + ein Subnetz darin (kein eigenes Netzwerk nötig für einen
# Einzel-Host-Benchmark).
data "aws_vpc" "default" {
  default = true
}

data "aws_subnets" "default" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.default.id]
  }
}

resource "aws_key_pair" "duel" {
  key_name   = "tentris-wdbench-key"
  public_key = file(var.ssh_public_key_path)
}

resource "aws_security_group" "ssh" {
  name        = "tentris-wdbench-ssh"
  description = "SSH-only inbound for the WDBench duel host"
  vpc_id      = data.aws_vpc.default.id

  ingress {
    description = "SSH"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = var.ssh_ingress_cidrs
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = { project = "tentris-wdbench" }
}

resource "aws_instance" "duel" {
  ami                         = data.aws_ami.ubuntu.id
  instance_type               = var.instance_type
  key_name                    = aws_key_pair.duel.key_name
  vpc_security_group_ids      = [aws_security_group.ssh.id]
  subnet_id                   = data.aws_subnets.default.ids[0]
  associate_public_ip_address = true

  # Großes, schnelles Root-Volume: dekomprimierte .nt (~100 GB) + Rust-Snapshot
  # (~50 GB) + Tentris-metall (~1,3 TB bei vollem Datensatz).
  root_block_device {
    volume_type = "gp3"
    volume_size = var.disk_gb
    throughput  = 500
    iops        = 12000
    encrypted   = true
  }

  tags = { project = "tentris-wdbench", Name = "tentris-wdbench-duel" }
}

output "server_ip" {
  description = "Public IPv4 of the WDBench duel host"
  value       = aws_instance.duel.public_ip
}

output "instance_type" {
  value = aws_instance.duel.instance_type
}
