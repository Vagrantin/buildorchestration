#!/bin/bash
set -e 

CONFIG_FILE="build.config"

echo "=========================================================="
echo "  XCP-ng AI Orchestrator Packer VM Generation Script       "
echo "=========================================================="

# 1. Load Configurations
if [ -f "$CONFIG_FILE" ]; then
    echo "---> Loading environment parameters from $CONFIG_FILE..."
    source "./$CONFIG_FILE"
else
    echo "ERROR: Configuration file '$CONFIG_FILE' not found! Exiting."
    exit 1
fi

# 2. Host System Preparation (Install Packer & Tools)
if [ -f /etc/os-release ]; then
    . /etc/os-release
    REPO_CODENAME=${UBUNTU_CODENAME:-$VERSION_CODENAME}
else
    REPO_CODENAME=$(lsb_release -cs)
fi
# Safeguard for Mint variants based on Ubuntu Noble
if [ "$REPO_CODENAME" = "zara" ]; then REPO_CODENAME="noble"; fi

echo "---> Syncing host repositories and dependencies..."
sudo apt-get update || true
sudo apt-get install -y wget gpg coreutils curl ufw

wget -O- https://apt.releases.hashicorp.com/gpg | sudo gpg --dearmor --yes -o /usr/share/keyrings/hashicorp-archive-keyring.gpg
echo "deb [signed-by=/usr/share/keyrings/hashicorp-archive-keyring.gpg] https://apt.releases.hashicorp.com ${REPO_CODENAME} main" | sudo tee /etc/apt/sources.list.d/hashicorp.list

sudo apt-get update && sudo apt-get install -y packer

echo "---> Initializing XCP-ng/XenServer Packer Plugin..."
packer plugins install github.com/ddelnano/xenserver

# 3. Open local ports dynamically for Packer's built-in HTTP preseed engine
sudo ufw allow 8000:9000/tcp || true

# 4. Resolve Remote Checksums Dynamically
echo "---> Fetching SHA256 verification string for the installer image..."
ISO_FILENAME=$(basename "$DEBIAN_ISO_URL")
ISO_BASE_URL=$(dirname "$DEBIAN_ISO_URL")
SHA256_CONTENT=$(curl -sSL "${ISO_BASE_URL}/SHA256SUMS" || echo "")
RAW_HASH=$(echo "$SHA256_CONTENT" | grep "$ISO_FILENAME" | head -n 1 | awk '{print $1}')

if [ -n "$RAW_HASH" ]; then
    DEBIAN_ISO_CHECKSUM="sha256:${RAW_HASH}"
    echo "Parsed Checksum: $DEBIAN_ISO_CHECKSUM"
else
    echo "Failed to query live checksum. Falling back to static template signature."
    DEBIAN_ISO_CHECKSUM="file:https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/SHA256SUMS"
fi

# 5. Scaffold Project Directives
BUILD_DIR="./packer-runtime"
mkdir -p "$BUILD_DIR/http"
cd "$BUILD_DIR"

# Write Automated Debian Installation Blueprint (Preseed)
cat << EOF > http/preseed.cfg
d-i debian-installer/locale string en_US
d-i debian-installer/add-kernel-opts string net.ifnames=0 biosdevname=0
d-i keyboard-configuration/xkb-keymap select us
d-i netcfg/choose_interface select auto
d-i netcfg/get_hostname string ai-orchestrator
d-i netcfg/get_domain string local
d-i mirror/country string manual
d-i mirror/http/hostname string deb.debian.org
d-i mirror/http/directory string /debian
d-i passwd/root-login boolean true
d-i passwd/root-password password ${DEBIAN_ROOT_PASSWORD}
d-i passwd/root-password-again password ${DEBIAN_ROOT_PASSWORD}
d-i passwd/user-fullname string Agent Orchestrator
d-i passwd/username string ${DEBIAN_SUDO_USER}
d-i passwd/user-password password ${DEBIAN_USER_PASSWORD}
d-i passwd/user-password-again password ${DEBIAN_USER_PASSWORD}
d-i clock-setup/utc boolean true
d-i time/zone string Asia/Tokyo
d-i partman-auto/method string regular
d-i partman-auto/filesystem string ext4
d-i partman-auto/choose_recipe select atomic
d-i partman-partitioning/confirm_write_new_label boolean true
d-i partman/choose_partition select finish
d-i partman/confirm boolean true
d-i partman/confirm_nooverwrite boolean true
tasksel tasksel/first multiselect minimal
d-i pkgsel/include string openssh-server sudo curl wget vim git jq build-essential ca-certificates network-manager
d-i pkgsel/exclude string wireless-tools wpagui bluetooth bluez lvm2
d-i grub-installer/only_debian boolean true
d-i grub-installer/with_other_os boolean true
d-i grub-installer/bootdev string default
d-i preseed/late_command string in-target sed -i 's/.*PermitRootLogin.*/PermitRootLogin yes/g' /etc/ssh/sshd_config ; echo "${DEBIAN_SUDO_USER} ALL=(ALL) NOPASSWD:ALL" >> /target/etc/sudoers.d/${DEBIAN_SUDO_USER}
d-i finish-install/reboot_in_progress note
EOF

# Write HCL Engine Infrastructure Design Block
cat << EOF > template.pkr.hcl
packer {
  required_plugins {
    xenserver = {
      version = ">= v0.7.0"
      source  = "github.com/ddelnano/xenserver"
    }
  }
}

source "xenserver-iso" "orchestrator" {
  remote_host      = "${XCPNG_IP}"
  remote_username  = "${XCPNG_USER}"
  remote_password  = "${XCPNG_PASSWORD}"
  sr_name          = "${SR_NAME}"
  network_names    = ["${VM_NETWORK_NAME}"]
  
  vm_name          = "${VM_NAME}"
  vm_description   = "Headless Debian Orchestration Machine - RTX 3090 GPU Compute Enabled"
  vm_tags	   = ["GPU", "prod"]
  vcpus_max        = ${VM_CPU_COUNT}
  vm_memory        = ${VM_MEMORY_MB}
  disk_size        = ${VM_DISK_SIZE_MB}
  
  iso_url          = "${DEBIAN_ISO_URL}"
  iso_checksum     = "${DEBIAN_ISO_CHECKSUM}"
  http_directory   = "http"
  
  boot_wait        = "5s"
  boot_command     = [
    "<wait5><esc><wait>",
    "install <wait>",
    " preseed/url=http://{{ .HTTPIP }}:{{ .HTTPPort }}/preseed.cfg <wait>",
    " debian-installer=en_US.UTF-8 <wait>",
    " auto=true <wait>",
    " priority=critical <wait>",
    " locale=en_US.UTF-8 <wait>",
    " keyboard-configuration/xkb-keymap=us <wait>",
    " netcfg/choose_interface=auto <wait>",
    " noprompt quiet--- <enter>"
  ]
  
  ssh_username      = "root"
  ssh_password      = "${DEBIAN_ROOT_PASSWORD}"
  ssh_timeout       = "30m"
  keep_vm           = "always" # Retain VM to allow immediate startup on the hypervisor
  export_template   = "false"
  skip_set_template = "true"
}

build {
  sources = ["source.xenserver-iso.orchestrator"]

  # Provisioning Runtime environment, GPU dependencies, Toolchains, and AI infrastructure
  provisioner "shell" {
    inline = [
      "echo '==> Enabling non-free apt components for official NVIDIA Drivers...'",
      "sed -i 's/main/main contrib non-free non-free-firmware/g' /etc/apt/sources.list",
       
      "echo '==> Updating base system...'",
      "apt-get update && apt-get upgrade -y",
      "echo '==> Fetching stable Xen Guest Utilities...'",
      "wget -q \"https://github.com/xenserver/xe-guest-utilities/releases/download/v10.0.0/xe-guest-utilities_10.0.0-1_amd64.deb\" -O /tmp/xe-guest-utilities.deb",
      "dpkg -i /tmp/xe-guest-utilities.deb || apt-get install -f -y",
      "rm -f /tmp/xe-guest-utilities.deb",
      
      "echo '==> Deploying kernel headers and Nvidia drivers...'",
      "apt-get install -y linux-headers-amd64 nvidia-driver nvidia-cuda-toolkit firmware-misc-nonfree",
      
      "echo '==> Provisioning Containerization Stack (Docker Enterprise Components)...'",
      "install -m 0755 -d /etc/apt/keyrings",
      "curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc",
      "chmod a+r /etc/apt/keyrings/docker.asc",
      "echo \"deb [signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian bookworm stable\" | tee /etc/apt/sources.list.d/docker.list",
      "apt-get update && apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin",
      
      "echo '==> Injecting Native System Compiler Infrastructure (Rust Toolchain)...'",
      "curl --proto \"=https\" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y",
      
      "echo '==> Setting up Ollama Engine Ecosystem...'",
      "curl -fsSL https://ollama.com/install.sh | sh",
      
      "echo '==> Preloading context LLM (Qwen2.5-Coder)...'",
      "systemctl start ollama",
      "sleep 5",
      "ollama pull qwen3-coder:30b", # Pulled and ready inside local context storage
      
      "echo '==> Preparing cleaning strategies...'",
      "apt-get clean"
    ]
  }
}
EOF

# 6. Execute Validations and Build Phase
packer validate template.pkr.hcl
PACKER_LOG=1 packer build template.pkr.hcl

echo "=========================================================="
echo "  Success! Target Host VM Generated. "
echo "=========================================================="
