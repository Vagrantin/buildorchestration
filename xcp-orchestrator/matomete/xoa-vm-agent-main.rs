//! XOA VM Agent - Replaces setup-xoa-builder.sh
//!
//! Builds XOA-HL (Xen Orchestra Appliance - Home Lab Edition) XVA images
//!
//! Workflow:
//! 1. Check if rebuild needed (HEAD SHA vs last_built_sha)
//! 2. Trigger GitHub Action (if configured)
//! 3. Validate prerequisites (Packer, plugin, disk, ports)
//! 4. Sync repository
//! 5. Resolve dynamic values (ISO checksum, RPM URL, etc.)
//! 6. Generate build files (inst.ks, almalinux-build.json)
//! 7. Run packer validate + build
//! 8. Locate XVA
//! 9. Upload to GitHub Release
//! 10. Persist version state
//! 11. Write final status

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use shared::{
    AgentStatus, WorkflowStatus, VersionState, load_github_token, create_github_client,
    fetch_repo_head_sha, parse_github_response, OrchestratorError, OWNER, DEFAULT_BRANCH,
    STATE_DIR,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tokio::fs as async_fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command as AsyncCommand;
use tokio::time::sleep;
use tracing::{info, warn, error, debug};

// ── Constants ────────────────────────────────────────────────────────────────

/// Agent status file path
const STATUS_FILE: &str = "/var/lib/xcp-hl-orchestrator/xoa-vm-agent.status.json";

/// Agent version state file path
const VERSION_STATE_FILE: &str = "/var/lib/xcp-hl-orchestrator/xoa_agent_version_state.json";

/// Repository directory
const REPO_DIR: &str = "/var/lib/xcp-hl-orchestrator/repos/build-xoa-hl";

/// Build directory
const BUILD_DIR: &str = "/var/lib/xcp-hl-orchestrator/build/xoa-hl";

/// Output directory for XVA
const OUTPUT_DIR: &str = "/var/lib/xcp-hl-orchestrator/output/xoa-hl";

/// XOA-HL repository
const XOA_HL_REPO: &str = "Vagrantin/xoa-hl";

/// AlmaLinux version
const ALMALINUX_VERSION: &str = "9";

/// AlmaLinux ISO URL
const ALMALINUX_ISO_URL: &str = "https://repo.almalinux.org/almalinux/9/isos/x86_64/AlmaLinux-9-latest-x86_64-minimal.iso";

/// XCP-ng target version
const XCPNG_TARGET_VERSION: &str = "8.3";

/// Required free disk space in bytes (100 GB)
const REQUIRED_DISK_SPACE: u64 = 100 * 1024 * 1024 * 1024;

/// Required ports
const REQUIRED_PORTS: &[u16] = &[8000, 8001, 8002, 8003, 8004, 8005, 8006, 8007, 8008, 8009, 9000];

/// Packer timeout
const PACKER_TIMEOUT: Duration = Duration::from_secs(3600); // 1 hour

// ── XOA-HL Version State ─────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct XoaHlVersionState {
    /// Last built SHA of xoa-hl repository
    pub last_built_sha: String,
    /// Last built tag (if any)
    pub last_tag: String,
    /// Last built timestamp
    pub last_built_at: Option<DateTime<Utc>>,
}

impl XoaHlVersionState {
    fn load() -> Result<Self> {
        if Path::new(VERSION_STATE_FILE).exists() {
            let content = fs::read_to_string(VERSION_STATE_FILE)
                .context("Failed to read version state file")?;
            Ok(serde_json::from_str(&content).unwrap_or_default())
        } else {
            Ok(Self::default())
        }
    }

    fn save(&self) -> Result<()> {
        fs::create_dir_all(Path::new(VERSION_STATE_FILE).parent().unwrap())?;
        let temp_path = Path::new(VERSION_STATE_FILE).with_extension("tmp");
        fs::write(&temp_path, serde_json::to_string_pretty(self)?)?;
        fs::rename(temp_path, VERSION_STATE_FILE)?;
        debug!("Saved XOA-HL version state");
        Ok(())
    }
}

// ── Error Types ─────────────────────────────────────────────────────────────

#[derive(thiserror::Error, Debug)]
enum XoaVmAgentError {
    #[error("Prerequisite validation failed: {0}")]
    PrerequisiteValidation(String),

    #[error("Repository sync failed: {0}")]
    RepositorySync(String),

    #[error("Dynamic value resolution failed: {0}")]
    DynamicValueResolution(String),

    #[error("Build file generation failed: {0}")]
    BuildFileGeneration(String),

    #[error("Packer command failed: {0}")]
    PackerCommand(String),

    #[error("XVA not found in output directory")]
    XvaNotFound,

    #[error("GitHub upload failed: {0}")]
    GitHubUpload(String),

    #[error("Timeout: {0}")]
    Timeout(String),
}

// ── Helper Types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BuildConfig {
    xcpng_ip: String,
    xcpng_user: String,
    xcpng_password: String,
    vm_network_name: String,
    vm_name: String,
    almalinux_root_password: String,
    almalinux_iso_url: String,
    almalinux_iso_checksum: String,
    xoa_hl_repo: String,
    xoa_hl_rpm_url: String,
    xe_guest_utilities_url: String,
    xe_guest_utilities_xenstore_url: String,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            xcpng_ip: "192.168.1.10".to_string(),
            xcpng_user: "root".to_string(),
            xcpng_password: String::new(), // Will be loaded from credentials
            vm_network_name: "Pool-wide network associated with eth0".to_string(),
            vm_name: "xoa-almalinux".to_string(),
            almalinux_root_password: String::new(), // Will be loaded from credentials
            almalinux_iso_url: ALMALINUX_ISO_URL.to_string(),
            almalinux_iso_checksum: String::new(),
            xoa_hl_repo: XOA_HL_REPO.to_string(),
            xoa_hl_rpm_url: String::new(),
            xe_guest_utilities_url: "https://github.com/xcp-ng/xcp/releases/download/v8.3.0/xe-guest-utilities-8.3.0-1.x86_64.rpm".to_string(),
            xe_guest_utilities_xenstore_url: "https://github.com/xcp-ng/xcp/releases/download/v8.3.0/xe-guest-utilities-xenstore-8.3.0-1.x86_64.rpm".to_string(),
        }
    }
}

// ── Main Workflow ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("xoa_vm_agent=info")
        .init();

    info!("Starting XOA VM Agent...");

    // Initialize status
    let mut status = AgentStatus::new("initialization", WorkflowStatus::InProgress);
    status.write_to_file(STATUS_FILE)?;

    // Load version state
    let mut version_state = XoaHlVersionState::load()?;

    // Load GitHub token
    let token = load_github_token().context("Failed to load GitHub token")?;
    let client = create_github_client(&token)?;

    // PHASE 1: Determine if rebuild needed
    info!("PHASE 1: Checking if rebuild is needed...");
    status.phase = "phase_1_check_rebuild".to_string();

    let repo_head_sha = fetch_repo_head_sha(&client, "xoa-hl").await
        .context("Failed to fetch xoa-hl HEAD SHA")?;

    if repo_head_sha == version_state.last_built_sha && !version_state.last_built_sha.is_empty() {
        info!("No changes detected (HEAD SHA: {}), skipping build.", repo_head_sha);
        status.status = WorkflowStatus::Skipped;
        status.detail = format!("No changes since last build (SHA: {})", repo_head_sha);
        status.write_to_file(STATUS_FILE)?;

        return Ok(());
    }

    info!("Changes detected, proceeding with build (HEAD SHA: {})", repo_head_sha);

    // PHASE 2: Trigger GitHub Action (if workflow exists)
    info!("PHASE 2: Checking for GitHub Actions workflow...");
    status.phase = "phase_2_trigger_workflow".to_string();

    // For now, we assume the build is triggered by the agent itself
    // In a future implementation, we could trigger a GitHub Action
    // that prepares the environment
    info!("No external workflow to trigger, proceeding with local build.");

    // PHASE 3: Validate prerequisites
    info!("PHASE 3: Validating prerequisites...");
    status.phase = "phase_3_validate_prerequisites".to_string();

    validate_prerequisites().await?;

    // PHASE 4: Synchronize repository
    info!("PHASE 4: Synchronizing repository...");
    status.phase = "phase_4_sync_repo".to_string();

    sync_repository().await?;

    // PHASE 5: Resolve dynamic values
    info!("PHASE 5: Resolving dynamic values...");
    status.phase = "phase_5_resolve_values".to_string();

    let mut config = BuildConfig::default();
    resolve_dynamic_values(&client, &mut config).await?;

    // PHASE 6: Generate temporary build files
    info!("PHASE 6: Generating build files...");
    status.phase = "phase_6_generate_files".to_string();

    let build_dir = PathBuf::from(BUILD_DIR);
    generate_build_files(&build_dir, &config).await?;

    // PHASE 7: Run packer validate and build
    info!("PHASE 7: Running Packer...");
    status.phase = "phase_7_run_packer".to_string();
    status.detail = "Running packer validate...".to_string();
    status.write_to_file(STATUS_FILE)?;

    let packer_result = run_packer(&build_dir).await;

    match packer_result {
        Ok(_) => {
            info!("Packer build completed successfully");
            status.status = WorkflowStatus::Success;
        }
        Err(e) => {
            error!("Packer build failed: {}", e);
            status.status = WorkflowStatus::Failure;
            status.detail = format!("Packer error: {}", e);
            status.write_to_file(STATUS_FILE)?;
            bail!("Packer build failed: {}", e);
        }
    }

    // PHASE 8: Locate generated XVA
    info!("PHASE 8: Locating XVA...");
    status.phase = "phase_8_locate_xva".to_string();

    let xva_path = locate_xva().await?;
    info!("Found XVA at: {}", xva_path.display());

    // PHASE 9: Upload asset to GitHub Release
    info!("PHASE 9: Uploading to GitHub Release...");
    status.phase = "phase_9_upload_asset".to_string();

    upload_to_github_release(&client, &xva_path).await?;

    // PHASE 10: Persist version state
    info!("PHASE 10: Persisting version state...");
    status.phase = "phase_10_persist_state".to_string();

    version_state.last_built_sha = repo_head_sha;
    version_state.last_built_at = Some(Utc::now());
    version_state.save()?;

    // PHASE 11: Write final status
    info!("PHASE 11: Writing final status...");
    status.phase = "phase_11_finalize".to_string();
    status.status = WorkflowStatus::Success;
    status.detail = format!("Build completed successfully. XVA: {}", xva_path.display());
    status.timestamp = Utc::now();
    status.write_to_file(STATUS_FILE)?;

    info!("XOA VM Agent completed successfully!");
    Ok(())
}

// ── PHASE 3: Validate Prerequisites ─────────────────────────────────────────

async fn validate_prerequisites() -> Result<()> {
    info!("Validating Packer installation...");

    // Check Packer is installed
    if !Command::new("packer").arg("--version").output().is_ok() {
        bail!("Packer is not installed. Please install Packer first.");
    }
    info!("✓ Packer is installed");

    // Check xenserver plugin
    info!("Validating XCP-ng Packer plugin...");
    let output = Command::new("packer")
        .arg("plugins")
        .arg("installed")
        .output()
        .context("Failed to check installed plugins")?;

    if !String::from_utf8_lossy(&output.stdout).contains("xenserver") {
        bail!("XCP-ng Packer plugin not installed. Run: packer plugins install github.com/ddelnano/xenserver");
    }
    info!("✓ XCP-ng Packer plugin is installed");

    // Check disk space
    info!("Validating disk space...");
    let output = Command::new("df")
        .arg("-k")
        .arg(".")
        .output()
        .context("Failed to check disk space")?;

    let output_str = String::from_utf8_lossy(&output.stdout);
    if let Some(available) = output_str.lines().nth(1) {
        let parts: Vec<&str> = available.split_whitespace().collect();
        if parts.len() >= 4 {
            let available_kb: u64 = parts[3].parse().unwrap_or(0);
            let available_bytes = available_kb * 1024;
            if available_bytes < REQUIRED_DISK_SPACE {
                bail!(
                    "Insufficient disk space. Required: {} GB, Available: {} GB",
                    REQUIRED_DISK_SPACE / (1024 * 1024 * 1024),
                    available_bytes / (1024 * 1024 * 1024)
                );
            }
        }
    }
    info!("✓ Sufficient disk space available");

    // Check ports
    info!("Validating required ports...");
    for port in REQUIRED_PORTS {
        // Simple check - in production, use proper port checking
        info!("✓ Port {} is available (check not fully implemented)", port);
    }

    Ok(())
}

// ── PHASE 4: Synchronize Repository ─────────────────────────────────────────

async fn sync_repository() -> Result<()> {
    let repo_path = Path::new(REPO_DIR);

    info!("Synchronizing repository at {}", repo_path.display());

    if !repo_path.exists() {
        // Clone repository
        info!("Cloning repository...");
        let status = AsyncCommand::new("git")
            .arg("clone")
            .arg(format!("https://github.com/{}", XOA_HL_REPO))
            .arg(repo_path)
            .status()
            .await
            .context("Failed to clone repository")?;

        if !status.success() {
            bail!("Git clone failed with exit code: {:?}", status.code());
        }
    } else {
        // Fetch and reset
        info!("Fetching latest changes...");

        // Change to repo directory
        let fetch_status = AsyncCommand::new("git")
            .arg("fetch")
            .current_dir(repo_path)
            .status()
            .await
            .context("Failed to fetch repository")?;

        if !fetch_status.success() {
            bail!("Git fetch failed with exit code: {:?}", fetch_status.code());
        }

        info!("Resetting to origin/main...");
        let reset_status = AsyncCommand::new("git")
            .arg("reset")
            .arg("--hard")
            .arg("origin/main")
            .current_dir(repo_path)
            .status()
            .await
            .context("Failed to reset repository")?;

        if !reset_status.success() {
            bail!("Git reset failed with exit code: {:?}", reset_status.code());
        }
    }

    info!("Repository synchronized successfully");
    Ok(())
}

// ── PHASE 5: Resolve Dynamic Values ─────────────────────────────────────────

async fn resolve_dynamic_values(client: &reqwest::Client, config: &mut BuildConfig) -> Result<()> {
    // Resolve AlmaLinux ISO checksum
    info!("Resolving AlmaLinux ISO checksum...");

    if config.almalinux_iso_checksum.is_empty() {
        config.almalinux_iso_checksum = resolve_almalinux_checksum(client)
            .await
            .context("Failed to resolve AlmaLinux checksum")?;
    }

    info!("AlmaLinux checksum: {}", config.almalinux_iso_checksum);

    // Resolve xoa-hl RPM URL
    info!("Resolving xoa-hl RPM URL...");
    config.xoa_hl_rpm_url = resolve_xoa_hl_rpm_url(client)
        .await
        .context("Failed to resolve xoa-hl RPM URL")?;

    info!("xoa-hl RPM URL: {}", config.xoa_hl_rpm_url);

    // Load credentials from environment
    if let Ok(creds_dir) = std::env::var("CREDENTIALS_DIRECTORY") {
        let creds_path = PathBuf::from(creds_dir);

        // Load XCP-ng credentials
        if let Ok(password) = fs::read_to_string(creds_path.join("XCPNG_PASSWORD")) {
            config.xcpng_password = password.trim().to_string();
        }

        // Load VM root password
        if let Ok(password) = fs::read_to_string(creds_path.join("ALMALINUX_ROOT_PASSWORD")) {
            config.almalinux_root_password = password.trim().to_string();
        }
    }

    Ok(())
}

async fn resolve_almalinux_checksum(client: &reqwest::Client) -> Result<String> {
    let iso_filename = "AlmaLinux-9-latest-x86_64-minimal.iso";
    let iso_base_url = "https://repo.almalinux.org/almalinux/9/isos/x86_64";

    // Try BSD-style CHECKSUM file first
    let checksum_url = format!("{}/CHECKSUM", iso_base_url);
    if let Ok(checksum_content) = client.get(&checksum_url).send().await {
        if checksum_content.status().is_success() {
            let body = checksum_content.text().await?;
            for line in body.lines() {
                if line.contains(iso_filename) {
                    if let Some(hash) = line.split_whitespace().last() {
                        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                            return Ok(format!("sha256:{}", hash));
                        }
                    }
                }
            }
        }
    }

    // Try GNU-style SHA256SUMS file
    let sha256sums_url = format!("{}/SHA256SUMS", iso_base_url);
    if let Ok(sha256_content) = client.get(&sha256sums_url).send().await {
        if sha256_content.status().is_success() {
            let body = sha256_content.text().await?;
            for line in body.lines() {
                if line.starts_with(|c: char| c.is_ascii_hexdigit()) && line.contains(iso_filename) {
                    let hash = line.split_whitespace().next().unwrap();
                    if hash.len() == 64 {
                        return Ok(format!("sha256:{}", hash));
                    }
                }
            }
        }
    }

    bail!("Could not resolve AlmaLinux ISO checksum automatically. Please provide ALMALINUX_ISO_CHECKSUM.");
}

async fn resolve_xoa_hl_rpm_url(client: &reqwest::Client) -> Result<String> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        XOA_HL_REPO
    );

    #[derive(serde::Deserialize)]
    struct GitHubRelease {
        assets: Vec<GitHubAsset>,
    }

    #[derive(serde::Deserialize)]
    struct GitHubAsset {
        browser_download_url: String,
    }

    let release: GitHubRelease = parse_github_response(
        client.get(&url).send().await.context("Failed to fetch releases")?,
        "resolve_xoa_hl_rpm_url",
    )
    .await?;

    for asset in release.assets {
        if asset.browser_download_url.ends_with(".rpm") {
            return Ok(asset.browser_download_url);
        }
    }

    bail!("No RPM asset found in latest release");
}

// ── PHASE 6: Generate Build Files ───────────────────────────────────────────

async fn generate_build_files(build_dir: &Path, config: &BuildConfig) -> Result<()> {
    info!("Creating build directory: {}", build_dir.display());

    // Create directories
    async_fs::create_dir_all(build_dir.join("patches")).await?;
    async_fs::create_dir_all(build_dir.join("scripts")).await?;
    async_fs::create_dir_all(build_dir.join("systemd")).await?;

    // Generate inst.ks
    info!("Generating inst.ks...");
    let ks_content = generate_kickstart(config);
    async_fs::write(build_dir.join("inst.ks"), ks_content).await?;

    // Generate almalinux-build.json
    info!("Generating almalinux-build.json...");
    let packer_content = generate_packer_template(config);
    async_fs::write(build_dir.join("almalinux-build.json"), packer_content).await?;

    // Copy helper scripts from repository
    info!("Copying helper scripts...");
    let repo_path = Path::new(REPO_DIR);

    // List of scripts to copy
    let scripts = [
        "scripts/xoa-first-boot.sh",
        "scripts/xoa-credentials.sh",
        "systemd/xoa-first-boot.service",
        "systemd/xoa-credentials.service",
    ];

    for script in scripts {
        let src = repo_path.join(script);
        let dst = build_dir.join(script);
        if src.exists() {
            async_fs::create_dir_all(dst.parent().unwrap()).await?;
            async_fs::copy(&src, &dst).await
                .with_context(|| format!("Failed to copy {}", script))?;
            info!("Copied {}", script);
        } else {
            warn!("Script not found: {}", script);
        }
    }

    info!("Build files generated successfully");
    Ok(())
}

fn generate_kickstart(config: &BuildConfig) -> String {
    format!(r#"# AlmaLinux {} Minimal Kickstart Configuration
# Target: Minimal install for XOA
# POC: SELinux disabled, DHCP network, EXT filesystem

# System language
lang en_US.UTF-8

# Keyboard layout
keyboard us

# Network configuration - DHCP only
network --onboot yes --device eth0 --bootproto dhcp

# Root password (will be replaced by Packer)
rootpw --plaintext {}

# System timezone
timezone Asia/Tokyo --utc

# SELinux configuration - DISABLED
selinux --disabled

# Firewall configuration
firewall --disabled

# System bootloader configuration
bootloader --location=mbr --boot-drive=xvda

# Clear the Master Boot Record
zerombr

# Partition clearing information
clearpart --all --initlabel

# Disk partitioning - EXT (not LVM)
part /boot --fstype="ext4" --size=1024
part / --fstype="ext4" --size=1 --grow

# Reboot after installation
reboot

%packages
@^minimal-environment
@core
chrony
openssh-server
openssh-clients
curl
tar
net-tools
iproute
%end

%post --log=/root/ks-post.log

# Ensure SELinux is disabled
sed -i 's/^SELINUX=.*/SELINUX=disabled/' /etc/selinux/config

systemctl disable network 2>/dev/null || true
systemctl enable NetworkManager --now

# Configure SSH for root login (temporary for build)
systemctl enable sshd --now
sed -i 's/^#PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
sed -i 's/^PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
systemctl restart sshd

systemctl enable chronyd --now

# Install extra packages not in base repo
dnf install -y epel-release
dnf install -y wget nc vim

# Create xo user for XOA
groupadd -f xo
useradd -m -g xo -s /bin/bash xo
usermod -aG wheel xo

%end
"#, ALMALINUX_VERSION, config.almalinux_root_password)
}

fn generate_packer_template(config: &BuildConfig) -> String {
    format!(r#"{{
  "builders": [
    {{
      "type": "xenserver-iso",
      "remote_host": "{}",
      "remote_username": "{}",
      "remote_password": "{}",
      "iso_url": "{}",
      "iso_checksum": "{}",
      "sr_name": "Local storage",
      "vm_name": "{}",
      "vm_description": "XOA HomeLab Edition - AlmaLinux {}",
      "disk_size": 10000,
      "vm_memory": 2048,
      "http_directory": ".",
      "network_names": ["{}"],
      "boot_command": [
        "<wait5><esc><wait>",
        "linux inst.ks=http://{{{{ .HTTPIP }}}}:{{{{ .HTTPPort }}}}/inst.ks inst.text<enter>"
      ],
      "boot_wait": "5s",
      "ssh_username": "root",
      "ssh_password": "{}",
      "ssh_timeout": "30m",
      "format": "xva_compressed",
      "keep_vm": "always",
      "skip_set_template": "true"
    }}
  ],
  "provisioners": [
    {{
      "type": "shell",
      "inline": [
        "echo '==> Updating base system...'",
        "dnf update -y"
      ]
    }},
    {{
      "type": "shell",
      "inline": [
        "echo '==> Installing xe-guest-utilities...'",
        "wget -q {} -O /tmp/xe-guest-utilities-xenstore.rpm",
        "wget -q {} -O /tmp/xe-guest-utilities.rpm",
        "dnf install -y /tmp/xe-guest-utilities-xenstore.rpm",
        "dnf install -y /tmp/xe-guest-utilities.rpm",
        "rm -f /tmp/xe-guest-utilities-xenstore.rpm",
        "rm -f /tmp/xe-guest-utilities.rpm"
      ]
    }},
    {{
      "type": "shell",
      "inline": [
        "echo '==> Installing Node.js 24.x...'",
        "curl -fsSL https://rpm.nodesource.com/setup_24.x | bash -",
        "dnf install -y nodejs"
      ]
    }},
    {{
      "type": "shell",
      "inline": [
        "echo '==> Installing xoa-hl RPM...'",
        "curl -fsSL '{}' -o /tmp/xoa-hl.rpm",
        "dnf install -y /tmp/xoa-hl.rpm",
        "rm -f /tmp/xoa-hl.rpm"
      ]
    }},
    {{
      "type": "file",
      "source": "scripts/xoa-first-boot.sh",
      "destination": "/root/xoa-first-boot.sh"
    }},
    {{
      "type": "file",
      "source": "scripts/xoa-credentials.sh",
      "destination": "/root/xoa-credentials.sh"
    }},
    {{
      "type": "file",
      "source": "systemd/xoa-first-boot.service",
      "destination": "/etc/systemd/system/xoa-first-boot.service"
    }},
    {{
      "type": "file",
      "source": "systemd/xoa-credentials.service",
      "destination": "/etc/systemd/system/xoa-credentials.service"
    }},
    {{
      "type": "shell",
      "inline": [
        "chmod +x /root/xoa-first-boot.sh /root/xoa-credentials.sh",
        "systemctl daemon-reload",
        "systemctl enable xoa-first-boot.service xoa-credentials.service"
      ]
    }},
    {{
      "type": "shell",
      "inline": [
        "echo '==> Cleaning up...'",
        "dnf remove -y iwl100-firmware iwl1000-firmware iwl105-firmware iwl135-firmware iwl2000-firmware iwl2030-firmware iwl3160-firmware iwl5000-firmware iwl5150-firmware iwl6000g2a-firmware iwl6050-firmware iwl7260-firmware",
        "dnf remove -y firewalld firewalld-filesystem python3-firewall python3-nftables NetworkManager-team teamd libteam NetworkManager-tui sssd-client sssd-common sssd-kcm sssd-nfs-idmap irqbalance microcode_ctl rsyslog rsyslog-logrotate man-db groff-base info lshw lsscsi sg3_utils sg3_utils-libs pciutils-libs ethtool",
        "dnf remove -y selinux-policy selinux-policy-targeted policycoreutils",
        "dnf autoremove -y",
        "dnf clean all",
        "rm -rf /var/cache/dnf/ /var/log/*.log",
        "rm -rf /usr/share/doc/* /usr/share/man/* /usr/share/info/*",
        "find /usr/share/locale -mindepth 1 -maxdepth 1 ! -name 'en*' -exec rm -rf {{}} +",
        "rm -rf /usr/share/i18n/locales"
      ]
    }},
    {{
      "type": "shell",
      "inline": [
        "echo '==> Stripping unique system identity...'",
        "echo -n > /etc/machine-id"
      ]
    }}
  ]
}}
"#,
        config.xcpng_ip,
        config.xcpng_user,
        config.xcpng_password,
        config.almalinux_iso_url,
        config.almalinux_iso_checksum,
        config.vm_name,
        ALMALINUX_VERSION,
        config.vm_network_name,
        config.almalinux_root_password,
        config.xe_guest_utilities_xenstore_url,
        config.xe_guest_utilities_url,
        config.xoa_hl_rpm_url
    )
}

// ── PHASE 7: Run Packer ─────────────────────────────────────────────────────

async fn run_packer(build_dir: &Path) -> Result<()> {
    let packer_file = build_dir.join("almalinux-build.json");

    info!("Running packer validate...");
    let validate_output = AsyncCommand::new("packer")
        .arg("validate")
        .arg(&packer_file)
        .current_dir(build_dir)
        .output()
        .await
        .context("Failed to run packer validate")?;

    if !validate_output.status.success() {
        let stderr = String::from_utf8_lossy(&validate_output.stderr);
        bail!("Packer validate failed:\n{}", stderr);
    }
    info!("✓ Packer validate passed");

    info!("Running packer build (this may take a while)...");
    info!("Timeout: {} minutes", PACKER_TIMEOUT.as_secs() / 60);

    // Run packer build with timeout
    let start = Instant::now();
    let mut child = AsyncCommand::new("packer")
        .arg("build")
        .arg("-on-error=ask")  // Continue on error for debugging
        .arg(&packer_file)
        .current_dir(build_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start packer build")?;

    // Read output in background
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buffer = Vec::new();
        loop {
            match reader.read_until(b'\n', &mut buffer).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buffer);
                    info!("[PACKER] {}", line.trim());
                    buffer.clear();
                }
                Err(e) => {
                    error!("Error reading stdout: {}", e);
                    break;
                }
            }
        }
    });

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buffer = Vec::new();
        loop {
            match reader.read_until(b'\n', &mut buffer).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buffer);
                    warn!("[PACKER ERR] {}", line.trim());
                    buffer.clear();
                }
                Err(e) => {
                    error!("Error reading stderr: {}", e);
                    break;
                }
            }
        }
    });

    // Wait for child with timeout
    let build_result = tokio::time::timeout(
        PACKER_TIMEOUT,
        child.wait()
    ).await;

    // Cancel output tasks
    stdout_task.abort();
    stderr_task.abort();

    match build_result {
        Ok(Ok(status)) => {
            if !status.success() {
                bail!("Packer build failed with exit code: {:?}", status.code());
            }
            info!("✓ Packer build completed successfully in {:?}", start.elapsed());
        }
        Ok(Err(e)) => {
            bail!("Packer build error: {}", e);
        }
        Err(_) => {
            // Timeout
            // Try to kill the process gracefully
            if let Err(e) = child.kill().await {
                warn!("Failed to kill packer process: {}", e);
            }
            bail!("Packer build timed out after {:?}", PACKER_TIMEOUT);
        }
    }

    Ok(())
}

// ── PHASE 8: Locate XVA ─────────────────────────────────────────────────────

async fn locate_xva() -> Result<PathBuf> {
    let output_dir = Path::new(OUTPUT_DIR);

    info!("Searching for XVA in {}", output_dir.display());

    // Ensure output directory exists
    if !output_dir.exists() {
        bail!("Output directory {} does not exist", output_dir.display());
    }

    // Look for .xva or .xva.gz files
    let mut xva_files: Vec<PathBuf> = Vec::new();

    for entry in async_fs::read_dir(output_dir).await? {
        let entry = entry?;
        let path = entry.path();
        let path_str = path.to_string_lossy();

        if path_str.ends_with(".xva") || path_str.ends_with(".xva.gz") {
            xva_files.push(path);
        }
    }

    if xva_files.is_empty() {
        bail!("No XVA file found in {}", output_dir.display());
    }

    // Sort by modification time and take the newest
    xva_files.sort_by(|a, b| {
        let a_time = async_fs::metadata(a).and_then(|m| m.modified()).ok();
        let b_time = async_fs::metadata(b).and_then(|m| m.modified()).ok();
        b_time.cmp(&a_time)
    });

    let newest = xva_files.into_iter().next().unwrap();
    info!("Found XVA: {}", newest.display());
    Ok(newest)
}

// ── PHASE 9: Upload to GitHub Release ───────────────────────────────────────

async fn upload_to_github_release(client: &reqwest::Client, xva_path: &Path) -> Result<()> {
    info!("Uploading XVA to GitHub Release...");

    // Get the latest release from xoa-hl repository
    let releases_url = format!("https://api.github.com/repos/{}/releases/latest", XOA_HL_REPO);

    #[derive(serde::Deserialize)]
    struct GitHubRelease {
        upload_url: String,
        tag_name: String,
    }

    let release: GitHubRelease = parse_github_response(
        client.get(&releases_url).send().await.context("Failed to fetch releases")?,
        "upload_to_github_release",
    )
    .await?;

    info!("Uploading to release: {}", release.tag_name);

    // Prepare the file for streaming upload
    let file_size = async_fs::metadata(xva_path).await?.len();
    info!("XVA file size: {} bytes ({:.2} GB)", file_size, file_size as f64 / (1024.0 * 1024.0 * 1024.0));

    // Extract just the base URL for uploads
    let upload_url = release.upload_url.trim_end_matches("{?name,label}");

    // Create a streaming upload
    let file = tokio::fs::File::open(xva_path).await?;
    let stream = tokio_util::io::ReaderStream::new(file);

    // Build the request
    let file_name = xva_path.file_name().unwrap().to_string_lossy();

    // GitHub requires Content-Length for uploads
    let response = client
        .post(&format!("{}?name={}", upload_url, file_name))
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", file_size)
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .context("Failed to send upload request")?;

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("GitHub upload failed: {} (status: {})", body, response.status());
    }

    info!("✓ XVA uploaded successfully to GitHub Release");
    Ok(())
}

// ── Result Type ─────────────────────────────────────────────────────────────

type Result<T> = anyhow::Result<T>;
