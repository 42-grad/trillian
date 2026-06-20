terraform {
  required_providers {
    hcloud = {
      source  = "hetznercloud/hcloud"
      version = "~> 1.45"
    }
  }
}

provider "hcloud" {
  token = var.hcloud_token
}

resource "hcloud_ssh_key" "default" {
  name       = "tentris-duel-key"
  public_key = file(var.ssh_public_key_path)
}

resource "hcloud_firewall" "ssh" {
  name = "tentris-duel-ssh"

  rule {
    direction  = "in"
    protocol   = "tcp"
    port       = "22"
    source_ips = ["0.0.0.0/0", "::/0"]
  }
}

resource "hcloud_server" "duel" {
  name         = "tentris-duel"
  server_type  = var.server_type
  image        = var.image
  location     = var.location
  ssh_keys     = [hcloud_ssh_key.default.id]
  firewall_ids = [hcloud_firewall.ssh.id]

  labels = {
    project = "tentris-duel"
  }
}

output "server_ip" {
  description = "IPv4 address of the duel server"
  value       = hcloud_server.duel.ipv4_address
}
