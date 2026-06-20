variable "hcloud_token" {
  description = "Hetzner Cloud API Token"
  type        = string
  sensitive   = true
}

variable "ssh_public_key_path" {
  description = "Path to the SSH public key to provision on the server"
  type        = string
  default     = "~/.ssh/id_rsa.pub"
}

variable "server_type" {
  description = "Hetzner Cloud server type (x86_64 recommended for Tentris)"
  type        = string
  default     = "cpx42"
}

variable "location" {
  description = "Hetzner Cloud datacenter location"
  type        = string
  default     = "nbg1"
}

variable "image" {
  description = "Hetzner Cloud OS image"
  type        = string
  default     = "ubuntu-24.04"
}
