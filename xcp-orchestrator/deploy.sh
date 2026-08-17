#!/bin/bash
set -e

echo "=========================================================="
echo "  Deploying/Updating XCP-orchestrator Rust Workspace       "
echo "=========================================================="

BINARIES=(orchestrator orchestrator-api iso-agent xoa-vm-agent)
UNITS=(
    xcp-orchestrator.service xcp-orchestrator.timer
    iso-agent.service        iso-agent.timer
    xoa-vm-agent.service     xoa-vm-agent.timer
    orchestrator-api.service
)
CREDS_DIR="/etc/xcp-hl-credentials"

# 1. Compile all workspace binaries in release mode locally
echo "---> Compiling optimized Rust binaries (workspace)..."
cargo build --release

# 2. Stop all timers, plus the long-running orchestrator-api service, before
#    swapping binaries — a running binary's file can't be overwritten in place.
echo "---> Stopping active timers and services..."
for unit in xcp-orchestrator.timer iso-agent.timer xoa-vm-agent.timer orchestrator-api.service; do
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
prompt_secret_if_missing "trigger_token"            "Enter a bearer token for the dashboard's manual trigger buttons"

# 5. Install the non-secret build config if not already present (never overwritten)
CONFIG_DIR="/etc/xcp-orchestrator"
sudo mkdir -p "$CONFIG_DIR"
if [ ! -f "$CONFIG_DIR/build.config" ]; then
    sudo cp xoa-vm-agent/build.config.sample "$CONFIG_DIR/build.config"
    sudo chmod 644 "$CONFIG_DIR/build.config"
    echo "NOTICE: installed default $CONFIG_DIR/build.config — review/edit it for your infrastructure."
fi

# 6. Sync systemd unit files
echo "---> Syncing systemd configurations..."
for unit in "${UNITS[@]}"; do
    sudo cp "systemd/${unit}" "/etc/systemd/system/"
done

# 7. Reload and activate all timer loops
sudo systemctl daemon-reload
for timer in xcp-orchestrator.timer iso-agent.timer xoa-vm-agent.timer; do
    sudo systemctl enable "$timer"
    sudo systemctl start "$timer"
done

# 8. (Re)start the manual-trigger API used by the dashboard's "Run now" buttons
sudo systemctl enable orchestrator-api.service
sudo systemctl restart orchestrator-api.service

# 9. Install and configure nginx: serves the static dashboard and proxies
#    /api/ to orchestrator-api (127.0.0.1:8787) for the "Run now" buttons.
echo "---> Installing and configuring nginx..."
if ! command -v nginx >/dev/null 2>&1; then
    sudo apt-get update
    sudo apt-get install -y nginx
fi

sudo mkdir -p /var/www/html/orchestrator
sudo cp nginx/orchestrator.conf /etc/nginx/sites-available/orchestrator
sudo ln -sf /etc/nginx/sites-available/orchestrator /etc/nginx/sites-enabled/orchestrator
# The stock "default" site also listens on :80 and would conflict with ours.
sudo rm -f /etc/nginx/sites-enabled/default

sudo nginx -t
sudo systemctl enable nginx
sudo systemctl reload nginx 2>/dev/null || sudo systemctl restart nginx

echo "=========================================================="
echo "  Redeployment Complete!"
echo "  Timer status checks:"
echo "    systemctl status xcp-orchestrator.timer"
echo "    systemctl status iso-agent.timer"
echo "    systemctl status xoa-vm-agent.timer"
echo "    systemctl status orchestrator-api.service"
echo "    systemctl status nginx"
echo ""
echo "  Force immediate manual runs:"
echo "    sudo systemctl start xcp-orchestrator.service"
echo "    sudo systemctl start iso-agent.service"
echo "    sudo systemctl start xoa-vm-agent.service"
echo ""
echo "  Dashboard: http://<this host>/  (nginx serves /var/www/html/orchestrator"
echo "  and proxies /api/ to orchestrator-api on 127.0.0.1:8787)"
echo "=========================================================="
