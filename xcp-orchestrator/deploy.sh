#!/bin/bash
set -e

echo "=========================================================="
echo "  Deploying/Updating XCP-orchestrator Rust Workspace       "
echo "=========================================================="

BINARIES=(orchestrator iso-agent xoa-vm-agent)
UNITS=(
    xcp-orchestrator.service xcp-orchestrator.timer
    iso-agent.service        iso-agent.timer
    xoa-vm-agent.service     xoa-vm-agent.timer
)
CREDS_DIR="/etc/xcp-hl-credentials"

# 1. Compile all workspace binaries in release mode locally
echo "---> Compiling optimized Rust binaries (workspace)..."
cargo build --release

# 2. Stop all timers before swapping binaries
echo "---> Stopping active timers..."
for unit in xcp-orchestrator.timer iso-agent.timer xoa-vm-agent.timer; do
    sudo systemctl stop "$unit" 2>/dev/null || true
done

# 3. Install each binary
echo "---> Installing binaries..."
for bin in "${BINARIES[@]}"; do
    sudo cp "target/release/${bin}" "/usr/local/bin/${bin}"
    echo "     installed /usr/local/bin/${bin}"
done

# 4. Handle credential assets safely if not already explicitly present
sudo mkdir -p "$CREDS_DIR"
sudo chmod 700 "$CREDS_DIR"

prompt_secret_if_missing() {
    local filename="$1"
    local prompt_text="$2"
    local path="${CREDS_DIR}/${filename}"
    if [ ! -f "$path" ]; then
        echo "WARNING: ${path} not found!"
        read -sp "${prompt_text}: " USER_SECRET
        echo
        echo "$USER_SECRET" | sudo tee "$path" > /dev/null
        sudo chmod 600 "$path"
    fi
}

prompt_secret_if_missing "github_token"            "Enter your GitHub Personal Access Token (PAT)"
prompt_secret_if_missing "xcpng_password"           "Enter the XCP-ng host root password"
prompt_secret_if_missing "almalinux_root_password"  "Enter the AlmaLinux VM root password to bake into images"

# 5. Sync systemd unit files
echo "---> Syncing systemd configurations..."
for unit in "${UNITS[@]}"; do
    sudo cp "systemd/${unit}" "/etc/systemd/system/"
done

# 6. Reload and activate all timer loops
sudo systemctl daemon-reload
for timer in xcp-orchestrator.timer iso-agent.timer xoa-vm-agent.timer; do
    sudo systemctl enable "$timer"
    sudo systemctl start "$timer"
done

echo "=========================================================="
echo "  Redeployment Complete!"
echo "  Timer status checks:"
echo "    systemctl status xcp-orchestrator.timer"
echo "    systemctl status iso-agent.timer"
echo "    systemctl status xoa-vm-agent.timer"
echo ""
echo "  Force immediate manual runs:"
echo "    sudo systemctl start xcp-orchestrator.service"
echo "    sudo systemctl start iso-agent.service"
echo "    sudo systemctl start xoa-vm-agent.service"
echo "=========================================================="
