// Candidate tool image. No product source, build outputs, test results or credentials.
packer {
  required_version = "= 1.16.2"
  required_plugins {
    tart = {
      source  = "github.com/cirruslabs/tart"
      version = "= 1.21.0"
    }
  }
}

variable "vm_name" {
  type        = string
  description = "A new, unused local VM name for this tool-image build."
  validation {
    condition     = can(regex("^kaban-t5e-tools-[a-z0-9-]+$", var.vm_name))
    error_message = "Use a new name beginning kaban-t5e-tools-."
  }
}

source "tart-cli" "kaban_tools" {
  vm_base_name = "ghcr.io/cirruslabs/ubuntu@sha256:92f2ea7463d8ae86f061d11d69696ad0b1053a59a958ecf941eebfa83534e700"
  vm_name      = var.vm_name
  cpu_count    = 4
  memory_gb    = 8
  disk_size_gb = 50
  headless     = true
  disable_vnc  = true
  ssh_username = "admin"
  ssh_password = "admin" // Public stock-image credentials, not repository credentials.
  ssh_timeout  = "10m"
}

build {
  sources = ["source.tart-cli.kaban_tools"]

  provisioner "shell" {
    inline = ["test ! -e /tmp/kaban-ci-inputs && mkdir /tmp/kaban-ci-inputs"]
  }
  provisioner "file" {
    source      = "${path.root}/inputs/"
    destination = "/tmp/kaban-ci-inputs/"
  }
  provisioner "shell" {
    script = "${path.root}/provision-tools.sh"
  }
}
