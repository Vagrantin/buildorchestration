# orchestratorhost

Builds the builder: a Debian VM on an XCP-ng hypervisor, provisioned with HashiCorp Packer
and pre-configured with GPU-passthrough compute (NVIDIA drivers, CUDA), Docker, and a local Ollama
instance for the log diagnostics the orchestrator performs.

This is a one-off: once the host exists, deploy the agents onto it from
[`../xcp-orchestrator`](../xcp-orchestrator/README.md).

## Configuration

Copy `build.config.sample` to `build.config` in this directory and fill in your
infrastructure details:

```bash
# XCP-ng Hypervisor Connection
XCPNG_IP="192.168.0.1"
XCPNG_USER="root"
XCPNG_PASSWORD="YOUR_SECRET_HYPERVISOR_PASSWORD"
SR_NAME="Local storage"
VM_NETWORK_NAME="LAN"

# Target Orchestrator VM Hardware Specs
VM_NAME="xcp-ai-orchestrator"
VM_CPU_COUNT=8
VM_MEMORY_MB=16384      # 16GB Allocation
VM_DISK_SIZE_MB=51200   # 50GB Space

# Target VM OS Security Accounts
DEBIAN_ROOT_PASSWORD="YOUR_SECURE_ROOT_PASSWORD"
DEBIAN_SUDO_USER="orchestrator"
DEBIAN_USER_PASSWORD="YOUR_SECURE_USER_PASSWORD"

# Installation Media Source
DEBIAN_ISO_URL="${DEBIAN_ISO_URL:-https://ftp.jaist.ac.jp/pub/Linux/debian-cd/current/amd64/iso-cd/debian-13.5.0-amd64-netinst.iso}"
```

## Running the build

The script verifies requirements, updates the local image-template configuration,
downloads the `ddelnano/xenserver` Packer plugin, resolves the installation
media checksum, and starts the build.

```bash
cd orchestratorhost/
chmod +x build-orchestrator.sh
./build-orchestrator.sh
```

## What lands on the target VM

* Core packages (`openssh-server`, `sudo`, `curl`, `wget`, `jq`, `build-essential`).
* XCP-ng Guest Utilities (`xe-guest-utilities`).
* NVIDIA non-free drivers and `nvidia-cuda-toolkit`, for GPU passthrough compute.
* Docker engine and the supporting toolchain.
* A local `ollama` server preloaded with the `qwen3-coder:30b` model.
