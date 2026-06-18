#!/bin/bash
set -e

echo "=========================================================="
echo "  Deploying/Updating XCP-ng Orchestrator Rust Agent       "
echo "=========================================================="

# 1. Compile the Binary in Release Mode locally
echo "---> Compiling optimized Rust binary..."
cargo build --release

# 2. Stage System Components
echo "---> Installing binary execution trees..."
sudo systemctl stop xcp-orchestrator.timer || true
sudo cp target/release/xcp-orchestrator /usr/local/bin/xcp-orchestrator

# 3. Handle Token Assets safely if not already explicitly present
sudo mkdir -p /etc/xcp-orchestrator
if [ ! -f /etc/xcp-orchestrator/github.token ]; then
    echo "WARNING: /etc/xcp-orchestrator/github.token not found!"
    read -sp "Enter your GitHub Personal Access Token (PAT): " USER_TOKEN
    echo
    echo "$USER_TOKEN" | sudo tee /etc/xcp-orchestrator/github.token > /dev/null
    sudo chmod 600 /etc/xcp-orchestrator/github.token
fi

# 4. Sync Systemd Unit Files
echo "---> Syncing systemd configurations..."
sudo cp systemd/xcp-orchestrator.service /etc/systemd/system/
sudo cp systemd/xcp-orchestrator.timer /etc/systemd/system/

# 5. Reload and Activate Timer Loops
sudo systemctl daemon-reload
sudo systemctl enable xcp-orchestrator.timer
sudo systemctl start xcp-orchestrator.timer

echo "=========================================================="
echo "  Redeployment Complete! "
echo "  Timer Status check: systemctl status xcp-orchestrator.timer"
echo "  Force immediate manual run: sudo systemctl start xcp-orchestrator.service"
echo "=========================================================="
