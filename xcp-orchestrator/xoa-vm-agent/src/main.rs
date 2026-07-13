//! XOA VM Agent - Replaces setup-xoa-builder.sh
//!
//! Builds XOA-HL (Xen Orchestra Appliance - Home Lab Edition) XVA images.
//!
//! Workflow:
//!  1. Check if rebuild needed (HEAD SHA vs last_built_sha)
//!  2. Trigger GitHub Actions workflow and wait for RPM build to complete
//!  3. Validate prerequisites (Packer, plugin, disk, ports)
//!  4. Sync repository
//!  5. Resolve dynamic values (ISO checksum, RPM URL, credentials)
//!  6. Generate temporary build files (inst.ks, almalinux-build.json)
//!  7. Run packer validate + build
//!  8. Locate generated XVA
//!  9. Create GitHub Release and upload XVA
//! 10. Persist version state
//! 11. Write final status

use anyhow::{Context, Result, bail};

use chrono::{DateTime, Utc};
use shared::{
    AgentStatus, WorkflowStatus,
    create_github_client, load_github_token,
    fetch_repo_head_sha, fetch_latest_release_ref,
    locate_dispatch_triggered_run, query_run_conclusion,
    parse_github_response,
};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::fs as async_fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener; // FIX #14: real port check via bind attempt
use tokio::process::Command as AsyncCommand;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

// ── Constants ────────────────────────────────────────────────────────────────

const STATUS_FILE: &str = "/var/lib/xcp-hl-orchestrator/xoa-vm-agent.status.json";
const VERSION_STATE_FILE: &str = "/var/lib/xcp-hl-orchestrator/xoa_agent_version_state.json";
const REPO_DIR: &str = "/var/lib/xcp-hl-orchestrator/repos/build-xoa-hl";
const BUILD_DIR: &str = "/var/lib/xcp-hl-orchestrator/build/xoa-hl";
const OUTPUT_DIR: &str = "/var/lib/xcp-hl-orchestrator/output/xoa-hl";

/// Full "owner/repo" path — used directly in API URLs that do not go through
/// the shared helpers (which prepend OWNER themselves).
const XOA_HL_REPO: &str = "Vagrantin/xoa-hl";
const BUILD_XOA_HL_REPO: &str = "Vagrantin/build-xoa-hl";

/// Workflow file that builds the RPM and creates a GitHub Release.
const XOA_HL_WORKFLOW_FILE: &str = "build-xoa.yml";

const ALMALINUX_VERSION: &str = "9";
const ALMALINUX_ISO_URL: &str =
    "https://repo.almalinux.org/almalinux/9/isos/x86_64/AlmaLinux-9-latest-x86_64-minimal.iso";

/// 100 GB minimum free space for build + output artefacts.
const REQUIRED_DISK_SPACE_BYTES: u64 = 100 * 1024 * 1024 * 1024;

/// Ports Packer's embedded HTTP server may bind to (kickstart delivery).
/// All must be free at build time.
const REQUIRED_PORTS: &[u16] = &[8000, 8001, 8002, 8003, 8004, 8005, 8006, 8007, 8008, 8009, 9000];

/// Hard timeout for the full Packer build.
const PACKER_TIMEOUT: Duration = Duration::from_secs(3600);

/// Grace period between SIGTERM and SIGKILL when Packer times out.
const PACKER_SIGTERM_GRACE: Duration = Duration::from_secs(30);

/// Hard timeout waiting for the xoa-hl GA workflow to complete.
const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(3600);

// ── Version State ─────────────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct XoaHlVersionState {
    pub last_built_sha: String,
    pub last_tag: String, // FIX #12: was never written — now updated in Phase 10
    pub last_built_at: Option<DateTime<Utc>>,
}

impl XoaHlVersionState {
    fn load() -> Result<Self> {
        if Path::new(VERSION_STATE_FILE).exists() {
            let content = std::fs::read_to_string(VERSION_STATE_FILE)
                .context("Failed to read XOA-HL version state")?;
            Ok(serde_json::from_str(&content).unwrap_or_default())
        } else {
            Ok(Self::default())
        }
    }

    fn save(&self) -> Result<()> {
        let path = Path::new(VERSION_STATE_FILE);
        std::fs::create_dir_all(path.parent().unwrap())
            .context("Failed to create state directory")?;
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, serde_json::to_string_pretty(self)?)
            .context("Failed to write version state temp file")?;
        std::fs::rename(&temp, path).context("Failed to rename version state file")?;
        debug!("XOA-HL version state saved");
        Ok(())
    }
}

// ── Error types kept for internal structured errors ───────────────────────────

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

// ── Build Config ──────────────────────────────────────────────────────────────

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
    xoa_hl_rpm_url: String,
    xe_guest_utilities_url: String,
    xe_guest_utilities_xenstore_url: String,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            xcpng_ip: "192.168.1.10".to_string(),
            xcpng_user: "root".to_string(),
            xcpng_password: String::new(),
            vm_network_name: "Pool-wide network associated with eth0".to_string(),
            vm_name: "xoa-almalinux".to_string(),
            almalinux_root_password: String::new(),
            almalinux_iso_url: ALMALINUX_ISO_URL.to_string(),
            almalinux_iso_checksum: String::new(),
            xoa_hl_rpm_url: String::new(),
            xe_guest_utilities_url:
                "https://github.com/xcp-ng/xcp/releases/download/v8.3.0/xe-guest-utilities-8.3.0-1.x86_64.rpm"
                    .to_string(),
            xe_guest_utilities_xenstore_url:
                "https://github.com/xcp-ng/xcp/releases/download/v8.3.0/xe-guest-utilities-xenstore-8.3.0-1.x86_64.rpm"
                    .to_string(),
        }
    }
}

// ── Main Workflow ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("xoa_vm_agent=info,warn")
        .init();

    info!("Starting XOA VM Agent...");

    let mut status = AgentStatus::new("initialization", WorkflowStatus::InProgress);
    status.write_to_file(STATUS_FILE)?;

    let mut version_state = XoaHlVersionState::load()?;
    let token = load_github_token().context("Failed to load GitHub token")?;
    let client = create_github_client(&token)?;

    // ── PHASE 1: Determine if rebuild is needed ──────────────────────────────
    info!("PHASE 1: Checking if rebuild is needed...");
    status.phase = "phase_1_check_rebuild".to_string();
    status.write_to_file(STATUS_FILE)?;

    let repo_head_sha = fetch_repo_head_sha(&client, "xoa-hl")
        .await
        .context("Failed to fetch xoa-hl HEAD SHA")?;

    if !version_state.last_built_sha.is_empty()
        && repo_head_sha == version_state.last_built_sha
    {
        info!(
            "No changes since last build (SHA: {}), skipping.",
            &repo_head_sha[..7]
        );
        status.status = WorkflowStatus::Skipped;
        status.detail = format!("No changes (SHA: {})", &repo_head_sha[..7]);
        status.write_to_file(STATUS_FILE)?;
        return Ok(());
    }

    // Local state is empty or stale — cross-check the latest GitHub release
    // (ground truth). This prevents pointless rebuilds after state loss.
    match fetch_latest_release_ref(&client, "xoa-hl").await {
        Ok(Some((tag, release_sha))) if release_sha == repo_head_sha => {
            info!(
                "No changes: latest release {} already matches HEAD (SHA: {}), skipping.",
                tag,
                &repo_head_sha[..7]
            );
            version_state.last_built_sha = repo_head_sha.clone();
            version_state.last_tag = tag.clone();
            version_state.save()?;
            status.status = WorkflowStatus::Skipped;
            status.detail = format!("Already released as {} (SHA: {})", tag, &repo_head_sha[..7]);
            status.write_to_file(STATUS_FILE)?;
            return Ok(());
        }
        Ok(_) => {}
        Err(e) => warn!(
            "Could not check latest xoa-hl release ({}); proceeding with build.",
            e
        ),
    }

    info!(
        "Changes detected (SHA: {}), proceeding with build.",
        &repo_head_sha[..7]
    );

    // ── PHASE 2: Trigger GA workflow and wait ────────────────────────────────
    info!("PHASE 2: Triggering xoa-hl build workflow...");
    status.phase = "phase_2_trigger_workflow".to_string();
    status.detail = "Dispatching xoa-hl GitHub Actions workflow".to_string();
    status.write_to_file(STATUS_FILE)?;

    let (run_id, run_url) = match trigger_xoa_hl_workflow(&client).await {
        Ok(r) => r,
        Err(e) => {
            error!("Workflow dispatch failed: {}", e);
            status.status = WorkflowStatus::Failure;
            status.detail = format!("Workflow dispatch failed: {}", e);
            status.set_component("xoa-hl", WorkflowStatus::Failure, String::new());
            status.write_to_file(STATUS_FILE)?;
            return Err(e);
        }
    };

    status.url = run_url.clone();
    status.detail = format!("Waiting for workflow: {}", run_url);
    status.set_component("xoa-hl", WorkflowStatus::InProgress, run_url.clone());
    status.write_to_file(STATUS_FILE)?;

    match wait_for_workflow(&client, run_id, &run_url, WORKFLOW_TIMEOUT).await {
        Ok(()) => {
            info!("xoa-hl workflow completed successfully");
            status.set_component("xoa-hl", WorkflowStatus::Success, run_url.clone());
        }
        Err(e) => {
            error!("xoa-hl workflow failed: {}", e);
            status.status = WorkflowStatus::Failure;
            status.detail = format!("Workflow failed: {}", e);
            status.set_component("xoa-hl", WorkflowStatus::Failure, run_url.clone());
            status.write_to_file(STATUS_FILE)?;
            return Err(e);
        }
    }

    // ── PHASE 3: Validate prerequisites ──────────────────────────────────────
    info!("PHASE 3: Validating prerequisites...");
    status.phase = "phase_3_validate_prerequisites".to_string();
    status.write_to_file(STATUS_FILE)?;

    if let Err(e) = validate_prerequisites().await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Prerequisite check failed: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 4: Sync repository ──────────────────────────────────────────────
    info!("PHASE 4: Synchronizing repository...");
    status.phase = "phase_4_sync_repo".to_string();
    status.write_to_file(STATUS_FILE)?;

    if let Err(e) = sync_repository().await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Repository sync failed: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 5: Resolve dynamic values ──────────────────────────────────────
    info!("PHASE 5: Resolving dynamic values...");
    status.phase = "phase_5_resolve_values".to_string();
    status.write_to_file(STATUS_FILE)?;

    let mut config = BuildConfig::default();
    if let Err(e) = resolve_dynamic_values(&client, &mut config).await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Value resolution failed: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 6: Generate build files ────────────────────────────────────────
    info!("PHASE 6: Generating build files...");
    status.phase = "phase_6_generate_files".to_string();
    status.write_to_file(STATUS_FILE)?;

    let build_dir = PathBuf::from(BUILD_DIR);
    if let Err(e) = generate_build_files(&build_dir, &config).await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Build file generation failed: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 7: Run Packer ───────────────────────────────────────────────────
    info!(
        "PHASE 7: Running Packer (timeout: {} min)...",
        PACKER_TIMEOUT.as_secs() / 60
    );
    status.phase = "phase_7_run_packer".to_string();
    status.detail = "Running packer validate + build".to_string();
    status.write_to_file(STATUS_FILE)?;

    if let Err(e) = run_packer(&build_dir).await {
        error!("Packer build failed: {}", e);
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Packer error: {}", e);
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    // ── PHASE 8: Locate XVA ───────────────────────────────────────────────────
    info!("PHASE 8: Locating generated XVA...");
    status.phase = "phase_8_locate_xva".to_string();
    status.write_to_file(STATUS_FILE)?;

    let xva_path = match locate_xva().await {
        Ok(p) => p,
        Err(e) => {
            status.status = WorkflowStatus::Failure;
            status.detail = format!("XVA not found: {}", e);
            status.write_to_file(STATUS_FILE)?;
            return Err(e);
        }
    };
    info!("XVA located: {}", xva_path.display());

    // ── PHASE 9: Create release and upload XVA ────────────────────────────────
    // FIX #11: was calling upload_to_github_release() which assumed a release
    //          already existed. Now we explicitly create (or reuse) one first.
    info!("PHASE 9: Creating GitHub Release and uploading XVA...");
    status.phase = "phase_9_upload_asset".to_string();
    status.write_to_file(STATUS_FILE)?;

    let image_tag = generate_image_tag(&repo_head_sha);
    let image_name = format!("XOA HomeLab Edition - {}", image_tag);

    let (upload_url, release_url) =
        match create_github_release(&client, &image_tag, &image_name, &repo_head_sha).await {
            Ok(u) => u,
            Err(e) => {
                status.status = WorkflowStatus::Failure;
                status.detail = format!("Release creation failed: {}", e);
                status.set_component("xoa-image", WorkflowStatus::Failure, String::new());
                status.write_to_file(STATUS_FILE)?;
                return Err(e);
            }
        };

    status.set_component("xoa-image", WorkflowStatus::InProgress, release_url.clone());
    status.write_to_file(STATUS_FILE)?;

    if let Err(e) = upload_asset(&client, &upload_url, &xva_path).await {
        status.status = WorkflowStatus::Failure;
        status.detail = format!("Asset upload failed: {}", e);
        status.set_component("xoa-image", WorkflowStatus::Failure, release_url.clone());
        status.write_to_file(STATUS_FILE)?;
        return Err(e);
    }

    status.set_component("xoa-image", WorkflowStatus::Success, release_url.clone());

    // ── PHASE 10: Persist version state ──────────────────────────────────────
    info!("PHASE 10: Persisting version state...");
    status.phase = "phase_10_persist_state".to_string();

    version_state.last_built_sha = repo_head_sha.clone();
    version_state.last_tag = image_tag.clone(); // FIX #12: was never set
    version_state.last_built_at = Some(Utc::now());
    version_state.save().context("Failed to persist version state")?;

    // ── PHASE 11: Write final status ──────────────────────────────────────────
    info!("PHASE 11: Finalizing...");
    status.phase = "phase_11_finalize".to_string();
    status.status = WorkflowStatus::Success;
    status.detail = format!(
        "Build complete. Tag: {}. XVA: {}",
        image_tag,
        xva_path.display()
    );
    status.timestamp = Utc::now();
    status.write_to_file(STATUS_FILE)?;

    info!("XOA VM Agent completed successfully! Tag: {}", image_tag);
    Ok(())
}

// ── PHASE 2 helpers ───────────────────────────────────────────────────────────

/// Dispatch the xoa-hl build workflow and wait for the run to appear in the API.
/// Returns `(run_id, html_url)`.
async fn trigger_xoa_hl_workflow(client: &reqwest::Client) -> Result<(u64, String)> {
    let url = format!(
        "https://api.github.com/repos/{}/actions/workflows/{}/dispatches",
        XOA_HL_REPO, XOA_HL_WORKFLOW_FILE,
    );

    let trigger_time = Utc::now();
    let payload = serde_json::json!({ "ref": "main" });

    let res = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .context("Failed to POST workflow dispatch")?;

    // workflow_dispatch returns 204 No Content on success
    if res.status() != reqwest::StatusCode::NO_CONTENT {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("workflow_dispatch returned {} — {}", code, body);
    }

    info!("Workflow dispatched, waiting for run to appear...");

    // FIX (P0-1): workflow_dispatch runs are filed under event=workflow_dispatch,
    // not event=push, and carry no head_branch/tag to match against. Using
    // locate_tag_triggered_run here (event=push filter) meant this call always
    // timed out after 180s. locate_dispatch_triggered_run polls the correct
    // event type and matches purely on created_at >= trigger_time.
    let (run_id, run_url) =
        locate_dispatch_triggered_run(client, "xoa-hl", trigger_time)
            .await
            .context("Failed to locate triggered workflow run")?;

    info!("Workflow run started: {}", run_url);
    Ok((run_id, run_url))
}

/// Poll a run until it completes or `timeout` elapses.
/// Consecutive status-poll failures tolerated before giving up — GitHub API
/// blips are routine over a long monitor and must not abort the build.
const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 5;

async fn wait_for_workflow(
    client: &reqwest::Client,
    run_id: u64,
    run_url: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut poll_failures: u32 = 0;

    loop {
        if Instant::now() > deadline {
            bail!(
                "Timed out after {:?} waiting for xoa-hl workflow ({})",
                timeout,
                run_url
            );
        }

        sleep(Duration::from_secs(30)).await;

        let conclusion = match query_run_conclusion(client, "xoa-hl", run_id).await {
            Ok(c) => {
                poll_failures = 0;
                c
            }
            Err(e) => {
                poll_failures += 1;
                warn!(
                    "xoa-hl status poll failed ({}/{}): {}",
                    poll_failures, MAX_CONSECUTIVE_POLL_FAILURES, e
                );
                if poll_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                    return Err(e).context("Failed to query workflow conclusion");
                }
                continue;
            }
        };

        info!("xoa-hl workflow: {}", conclusion);

        match conclusion.as_str() {
            "success" | "Success" => return Ok(()),
            "In Progress" => continue,
            other => bail!(
                "xoa-hl workflow ended with '{}' ({})",
                other,
                run_url
            ),
        }
    }
}

// ── PHASE 3: Validate Prerequisites ──────────────────────────────────────────

async fn validate_prerequisites() -> Result<()> {
    // FIX #16: was using blocking std::process::Command throughout.
    //          All checks now use AsyncCommand so the executor is not blocked.

    // Check Packer binary
    info!("Checking Packer...");
    let packer_out = AsyncCommand::new("packer")
        .arg("version")
        .output()
        .await
        .context("Packer binary not found — install from https://developer.hashicorp.com/packer/downloads")?;

    if !packer_out.status.success() {
        bail!(
            "Packer version check failed: {}",
            String::from_utf8_lossy(&packer_out.stderr)
        );
    }
    info!("Packer: {}", String::from_utf8_lossy(&packer_out.stdout).trim());

    // Check xenserver plugin
    info!("Checking xenserver Packer plugin...");
    let plugin_out = AsyncCommand::new("packer")
        .arg("plugins")
        .arg("installed")
        .output()
        .await
        .context("Failed to list Packer plugins")?;

    if !String::from_utf8_lossy(&plugin_out.stdout).contains("xenserver") {
        bail!(
            "XCP-ng Packer plugin not installed.\n\
             Run: packer plugins install github.com/ddelnano/xenserver"
        );
    }
    info!("xenserver plugin: present");

    // Check disk space
    info!("Checking disk space...");
    let df_out = AsyncCommand::new("df")
        .arg("-k")
        .arg(BUILD_DIR)
        .output()
        .await
        .context("Failed to run df")?;

    let df_text = String::from_utf8_lossy(&df_out.stdout);
    if let Some(line) = df_text.lines().nth(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 4 {
            let avail_kb: u64 = parts[3].parse().unwrap_or(0);
            let avail_bytes = avail_kb * 1024;
            if avail_bytes < REQUIRED_DISK_SPACE_BYTES {
                bail!(
                    "Insufficient disk space — required {}GB, have {}GB",
                    REQUIRED_DISK_SPACE_BYTES / (1024 * 1024 * 1024),
                    avail_bytes / (1024 * 1024 * 1024),
                );
            }
            info!(
                "Disk: {}GB available",
                avail_bytes / (1024 * 1024 * 1024)
            );
        }
    }

    // FIX #14: was a stub that unconditionally logged "port available".
    //          Now attempts a real bind; if the port is already in use the bind fails.
    info!("Checking required ports...");
    let mut blocked: Vec<u16> = Vec::new();
    for &port in REQUIRED_PORTS {
        match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
            Ok(_listener) => debug!("Port {} free", port),
            // listener drops here, releasing the port immediately
            Err(_) => blocked.push(port),
        }
    }
    if !blocked.is_empty() {
        bail!(
            "Required ports already in use: {:?}\n\
             Packer needs these free for its kickstart HTTP server.",
            blocked
        );
    }
    info!("All required ports are available");

    Ok(())
}

// ── PHASE 4: Sync repository ──────────────────────────────────────────────────

async fn sync_repository() -> Result<()> {
    let repo_path = Path::new(REPO_DIR);
    info!("Syncing {}", repo_path.display());

    if !repo_path.exists() {
        let status = AsyncCommand::new("git")
            .args(["clone", &format!("https://github.com/{}", BUILD_XOA_HL_REPO)])
            .arg(repo_path)
            .status()
            .await
            .context("git clone failed")?;
        if !status.success() {
            bail!("git clone exited with {:?}", status.code());
        }
    } else {
        let fetch = AsyncCommand::new("git")
            .arg("fetch")
            .current_dir(repo_path)
            .status()
            .await
            .context("git fetch failed")?;
        if !fetch.success() {
            bail!("git fetch exited with {:?}", fetch.code());
        }

        let reset = AsyncCommand::new("git")
            .args(["reset", "--hard", "origin/main"])
            .current_dir(repo_path)
            .status()
            .await
            .context("git reset failed")?;
        if !reset.success() {
            bail!("git reset exited with {:?}", reset.code());
        }
    }

    info!("Repository synchronized");
    Ok(())
}

// ── PHASE 5: Resolve dynamic values ──────────────────────────────────────────

async fn resolve_dynamic_values(
    client: &reqwest::Client,
    config: &mut BuildConfig,
) -> Result<()> {
    // Credentials come exclusively from systemd LoadCredential — never from env vars set manually
    let creds_dir = std::env::var("CREDENTIALS_DIRECTORY").map_err(|_| {
        anyhow::anyhow!(
            "CREDENTIALS_DIRECTORY not set — configure systemd LoadCredential= \
             for XCPNG_PASSWORD and ALMALINUX_ROOT_PASSWORD"
        )
    })?;
    let creds = std::path::PathBuf::from(creds_dir);

    config.xcpng_password = std::fs::read_to_string(creds.join("XCPNG_PASSWORD"))
        .context("XCPNG_PASSWORD credential missing")?
        .trim()
        .to_string();

    config.almalinux_root_password =
        std::fs::read_to_string(creds.join("ALMALINUX_ROOT_PASSWORD"))
            .context("ALMALINUX_ROOT_PASSWORD credential missing")?
            .trim()
            .to_string();

    info!("Resolving AlmaLinux ISO checksum...");
    config.almalinux_iso_checksum = resolve_almalinux_checksum(client)
        .await
        .context("Failed to resolve AlmaLinux checksum")?;
    info!("Checksum: {}", config.almalinux_iso_checksum);

    info!("Resolving xoa-hl RPM URL...");
    config.xoa_hl_rpm_url = resolve_xoa_hl_rpm_url(client)
        .await
        .context("Failed to resolve xoa-hl RPM URL")?;
    info!("RPM URL: {}", config.xoa_hl_rpm_url);

    Ok(())
}

async fn resolve_almalinux_checksum(client: &reqwest::Client) -> Result<String> {
    let iso_filename = "AlmaLinux-9-latest-x86_64-minimal.iso";
    let base = "https://repo.almalinux.org/almalinux/9/isos/x86_64";

    // Try BSD-style CHECKSUM first
    let checksum_url = format!("{}/CHECKSUM", base);
    if let Ok(res) = client.get(&checksum_url).send().await {
        if res.status().is_success() {
            let body = res.text().await?;
            for line in body.lines() {
                if line.contains(iso_filename) {
                    // BSD format: "SHA256 (filename) = <hash>"
                    if let Some(hash) = line.split_whitespace().last() {
                        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
                            return Ok(format!("sha256:{}", hash));
                        }
                    }
                }
            }
        }
    }

    // Fall back to GNU-style SHA256SUMS
    let sha256_url = format!("{}/SHA256SUMS", base);
    if let Ok(res) = client.get(&sha256_url).send().await {
        if res.status().is_success() {
            let body = res.text().await?;
            for line in body.lines() {
                if line.starts_with(|c: char| c.is_ascii_hexdigit())
                    && line.contains(iso_filename)
                {
                    // GNU format: "<hash>  filename"
                    if let Some(hash) = line.split_whitespace().next() {
                        if hash.len() == 64 {
                            return Ok(format!("sha256:{}", hash));
                        }
                    }
                }
            }
        }
    }

    bail!("Could not resolve AlmaLinux ISO checksum from official sources");
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
        client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch xoa-hl releases")?,
        "resolve_xoa_hl_rpm_url",
    )
    .await?;

    release
        .assets
        .into_iter()
        .find(|a| a.browser_download_url.ends_with(".rpm"))
        .map(|a| a.browser_download_url)
        .ok_or_else(|| anyhow::anyhow!("No RPM asset found in latest xoa-hl release"))
}

// ── PHASE 6: Generate build files ─────────────────────────────────────────────

async fn generate_build_files(build_dir: &Path, config: &BuildConfig) -> Result<()> {
    info!("Creating build directory: {}", build_dir.display());
    async_fs::create_dir_all(build_dir.join("patches")).await?;
    async_fs::create_dir_all(build_dir.join("scripts")).await?;
    async_fs::create_dir_all(build_dir.join("systemd")).await?;

    info!("Generating inst.ks...");
    async_fs::write(build_dir.join("inst.ks"), generate_kickstart(config)).await?;

    info!("Generating almalinux-build.json...");
    async_fs::write(
        build_dir.join("almalinux-build.json"),
        generate_packer_template(config),
    )
    .await?;

    // Copy helper scripts from the synced repo (non-fatal if missing)
    let scripts = [
        "scripts/xoa-first-boot.sh",
        "scripts/xoa-credentials.sh",
        "systemd/xoa-first-boot.service",
        "systemd/xoa-credentials.service",
    ];
    let repo = Path::new(REPO_DIR);
    for script in &scripts {
        let src = repo.join(script);
        let dst = build_dir.join(script);
        if src.exists() {
            async_fs::create_dir_all(dst.parent().unwrap()).await?;
            async_fs::copy(&src, &dst)
                .await
                .with_context(|| format!("Failed to copy {}", script))?;
            info!("Copied {}", script);
        } else {
            warn!("Script not found in repo, skipping: {}", script);
        }
    }

    info!("Build files generated");
    Ok(())
}

fn generate_kickstart(config: &BuildConfig) -> String {
    format!(
        r#"# AlmaLinux {ver} Minimal Kickstart — XOA HomeLab Edition
lang en_US.UTF-8
keyboard us
network --onboot yes --device eth0 --bootproto dhcp
rootpw --plaintext {rootpw}
timezone Asia/Tokyo --utc
selinux --disabled
firewall --disabled
bootloader --location=mbr --boot-drive=xvda
zerombr
clearpart --all --initlabel
part /boot --fstype="ext4" --size=1024
part / --fstype="ext4" --size=1 --grow
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
sed -i 's/^SELINUX=.*/SELINUX=disabled/' /etc/selinux/config
systemctl disable network 2>/dev/null || true
systemctl enable NetworkManager --now
systemctl enable sshd --now
sed -i 's/^#PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
sed -i 's/^PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
systemctl restart sshd
systemctl enable chronyd --now
dnf install -y epel-release
dnf install -y wget nc vim
groupadd -f xo
useradd -m -g xo -s /bin/bash xo
usermod -aG wheel xo
%end
"#,
        ver = ALMALINUX_VERSION,
        rootpw = config.almalinux_root_password,
    )
}

fn generate_packer_template(config: &BuildConfig) -> String {
    format!(
        r#"{{
  "builders": [{{
    "type": "xenserver-iso",
    "remote_host": "{xcpng_ip}",
    "remote_username": "{xcpng_user}",
    "remote_password": "{xcpng_pass}",
    "iso_url": "{iso_url}",
    "iso_checksum": "{iso_chk}",
    "sr_name": "Local storage",
    "vm_name": "{vm_name}",
    "vm_description": "XOA HomeLab Edition — AlmaLinux {ver}",
    "disk_size": 10000,
    "vm_memory": 2048,
    "http_directory": ".",
    "network_names": ["{net}"],
    "boot_command": [
      "<wait5><esc><wait>",
      "linux inst.ks=http://{{{{ .HTTPIP }}}}:{{{{ .HTTPPort }}}}/inst.ks inst.text<enter>"
    ],
    "boot_wait": "5s",
    "ssh_username": "root",
    "ssh_password": "{rootpw}",
    "ssh_timeout": "30m",
    "format": "xva_compressed",
    "keep_vm": "always",
    "skip_set_template": "true",
    "output_directory": "{output_dir}"
  }}],
  "provisioners": [
    {{"type":"shell","inline":["dnf update -y"]}},
    {{"type":"shell","inline":[
      "wget -q {xe_xenstore_url} -O /tmp/xe-guest-utilities-xenstore.rpm",
      "wget -q {xe_url} -O /tmp/xe-guest-utilities.rpm",
      "dnf install -y /tmp/xe-guest-utilities-xenstore.rpm /tmp/xe-guest-utilities.rpm",
      "rm -f /tmp/xe-guest-utilities*.rpm"
    ]}},
    {{"type":"shell","inline":[
      "curl -fsSL https://rpm.nodesource.com/setup_24.x | bash -",
      "dnf install -y nodejs"
    ]}},
    {{"type":"shell","inline":[
      "curl -fsSL '{rpm_url}' -o /tmp/xoa-hl.rpm",
      "dnf install -y /tmp/xoa-hl.rpm",
      "rm -f /tmp/xoa-hl.rpm"
    ]}},
    {{"type":"file","source":"scripts/xoa-first-boot.sh","destination":"/root/xoa-first-boot.sh"}},
    {{"type":"file","source":"scripts/xoa-credentials.sh","destination":"/root/xoa-credentials.sh"}},
    {{"type":"file","source":"systemd/xoa-first-boot.service","destination":"/etc/systemd/system/xoa-first-boot.service"}},
    {{"type":"file","source":"systemd/xoa-credentials.service","destination":"/etc/systemd/system/xoa-credentials.service"}},
    {{"type":"shell","inline":[
      "chmod +x /root/xoa-first-boot.sh /root/xoa-credentials.sh",
      "systemctl daemon-reload",
      "systemctl enable xoa-first-boot.service xoa-credentials.service"
    ]}},
    {{"type":"shell","inline":[
      "dnf remove -y firewalld firewalld-filesystem python3-firewall selinux-policy selinux-policy-targeted policycoreutils",
      "dnf autoremove -y",
      "dnf clean all",
      "rm -rf /var/cache/dnf/ /var/log/*.log /usr/share/doc/* /usr/share/man/*",
      "echo -n > /etc/machine-id"
    ]}}
  ]
}}"#,
        xcpng_ip = config.xcpng_ip,
        xcpng_user = config.xcpng_user,
        xcpng_pass = config.xcpng_password,
        iso_url = config.almalinux_iso_url,
        iso_chk = config.almalinux_iso_checksum,
        vm_name = config.vm_name,
        ver = ALMALINUX_VERSION,
        net = config.vm_network_name,
        rootpw = config.almalinux_root_password,
        xe_xenstore_url = config.xe_guest_utilities_xenstore_url,
        xe_url = config.xe_guest_utilities_url,
        rpm_url = config.xoa_hl_rpm_url,
        output_dir = OUTPUT_DIR,
    )
}

// ── PHASE 7: Run Packer ───────────────────────────────────────────────────────

async fn run_packer(build_dir: &Path) -> Result<()> {
    let packer_file = build_dir.join("almalinux-build.json");

    // Validate first — cheap and catches template errors before a long build
    info!("Running packer validate...");
    let validate = AsyncCommand::new("packer")
        .arg("validate")
        .arg(&packer_file)
        .current_dir(build_dir)
        .output()
        .await
        .context("Failed to run packer validate")?;

    if !validate.status.success() {
        bail!(
            "Packer validate failed:\n{}",
            String::from_utf8_lossy(&validate.stderr)
        );
    }
    info!("Packer validate passed");

    info!(
        "Running packer build (timeout: {} min)...",
        PACKER_TIMEOUT.as_secs() / 60
    );
    let start = Instant::now();

    let mut child = AsyncCommand::new("packer")
        .arg("build")
        // FIX #10: was "-on-error=ask" which blocks on stdin forever in automation.
        //          "-on-error=abort" tears down and exits immediately on failure.
        .arg("-on-error=abort")
        .arg(&packer_file)
        .current_dir(build_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn packer build")?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Stream packer output to the agent log without blocking the wait
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => info!("[packer] {}", line.trim_end()),
                Err(e) => {
                    error!("packer stdout read error: {}", e);
                    break;
                }
            }
        }
    });

    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => warn!("[packer:err] {}", line.trim_end()),
                Err(e) => {
                    error!("packer stderr read error: {}", e);
                    break;
                }
            }
        }
    });

    let build_result = tokio::time::timeout(PACKER_TIMEOUT, child.wait()).await;

    stdout_task.abort();
    stderr_task.abort();

    match build_result {
        Ok(Ok(exit)) if exit.success() => {
            info!("Packer build finished in {:?}", start.elapsed());
            Ok(())
        }
        Ok(Ok(exit)) => {
            bail!("Packer build failed (exit code: {:?})", exit.code());
        }
        Ok(Err(e)) => {
            bail!("Packer process error: {}", e);
        }
        Err(_timeout) => {
            // FIX #10 (continued): SIGTERM first, wait grace period, then SIGKILL.
            warn!(
                "Packer timed out after {:?}, sending SIGTERM...",
                PACKER_TIMEOUT
            );

            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // Safety: kill(2) is async-signal-safe; pid is valid.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            }

            match tokio::time::timeout(PACKER_SIGTERM_GRACE, child.wait()).await {
                Ok(_) => info!("Packer exited after SIGTERM"),
                Err(_) => {
                    warn!("Packer did not exit after SIGTERM, sending SIGKILL");
                    let _ = child.kill().await;
                }
            }

            bail!("Packer build timed out after {:?}", PACKER_TIMEOUT);
        }
    }
}

// ── PHASE 8: Locate XVA ───────────────────────────────────────────────────────

async fn locate_xva() -> Result<PathBuf> {
    let output_dir = Path::new(OUTPUT_DIR);

    if !output_dir.exists() {
        bail!("Output directory {} does not exist", output_dir.display());
    }

    // Step 1: collect candidate paths
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut dir = async_fs::read_dir(output_dir)
        .await
        .with_context(|| format!("Cannot read output dir {}", output_dir.display()))?;

    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        let name = path.to_string_lossy();
        if name.ends_with(".xva") || name.ends_with(".xva.gz") {
            candidates.push(path);
        }
    }

    if candidates.is_empty() {
        bail!("No .xva or .xva.gz file found in {}", output_dir.display());
    }

    // FIX #5: was calling async_fs::metadata() inside a sync sort_by closure.
    //         Collect mtimes first (async), then sort synchronously on the results.
    let mut with_mtime: Vec<(PathBuf, Option<std::time::SystemTime>)> = Vec::new();
    for path in candidates {
        let mtime = async_fs::metadata(&path)
            .await
            .ok()
            .and_then(|m| m.modified().ok());
        with_mtime.push((path, mtime));
    }

    // Newest first
    with_mtime.sort_by(|(_, a), (_, b)| b.cmp(a));

    let (path, _) = with_mtime.into_iter().next().unwrap();
    info!("Selected XVA: {}", path.display());
    Ok(path)
}

// ── PHASE 9: Create release and upload ────────────────────────────────────────

fn generate_image_tag(head_sha: &str) -> String {
    let date = Utc::now().format("%Y%m%d");
    let short = &head_sha[..7.min(head_sha.len())];
    format!("xoa-image-{}-{}", date, short)
}

/// Create a GitHub Release, or reuse the existing one (idempotent).
/// Returns `(upload_url, html_url)`.
///
/// The tag is created by the GitHub API using `target_sha` as `target_commitish`.
async fn create_github_release(
    client: &reqwest::Client,
    tag: &str,
    name: &str,
    target_sha: &str,
) -> Result<(String, String)> {
    #[derive(serde::Deserialize)]
    struct ReleaseResp {
        upload_url: String,
        html_url: String,
    }

    // Check whether this exact tag already has a release (retry-safe)
    let check_url = format!(
        "https://api.github.com/repos/{}/releases/tags/{}",
        XOA_HL_REPO, tag
    );
    if let Ok(res) = client.get(&check_url).send().await {
        if res.status().is_success() {
            if let Ok(r) = res.json::<ReleaseResp>().await {
                info!("Release {} already exists, reusing: {}", tag, r.html_url);
                return Ok((
                    r.upload_url.trim_end_matches("{?name,label}").to_string(),
                    r.html_url,
                ));
            }
        }
    }

    // Create a new release; the API creates the tag at target_sha automatically
    let create_url = format!(
        "https://api.github.com/repos/{}/releases",
        XOA_HL_REPO
    );
    let payload = serde_json::json!({
        "tag_name":          tag,
        "target_commitish":  target_sha,
        "name":              name,
        "body":              format!("XOA HomeLab Edition VM image\nSource commit: {}", target_sha),
        "draft":             false,
        "prerelease":        false,
    });

    let res = client
        .post(&create_url)
        .json(&payload)
        .send()
        .await
        .context("Failed to POST GitHub Release")?;

    if !res.status().is_success() {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("GitHub Release creation failed ({}): {}", code, body);
    }

    let release: ReleaseResp = res
        .json()
        .await
        .context("Failed to parse GitHub Release response")?;

    info!("Created GitHub Release: {}", release.html_url);
    Ok((
        release.upload_url.trim_end_matches("{?name,label}").to_string(),
        release.html_url,
    ))
}

/// Stream-upload `xva_path` to an existing GitHub Release upload URL.
async fn upload_asset(
    client: &reqwest::Client,
    upload_url: &str,
    xva_path: &Path,
) -> Result<()> {
    let file_size = async_fs::metadata(xva_path)
        .await
        .with_context(|| format!("Cannot stat {}", xva_path.display()))?
        .len();

    let file_name = xva_path
        .file_name()
        .context("XVA path has no filename")?
        .to_string_lossy()
        .to_string();

    info!(
        "Uploading {} ({:.2} GB)...",
        file_name,
        file_size as f64 / (1024.0_f64.powi(3))
    );

    let file = async_fs::File::open(xva_path)
        .await
        .with_context(|| format!("Cannot open {}", xva_path.display()))?;

    let stream = tokio_util::io::ReaderStream::new(file);

    let res = client
        .post(&format!("{}?name={}", upload_url, file_name))
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", file_size)
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .context("Upload request failed")?;

    if !res.status().is_success() {
        let code = res.status();
        let body = res.text().await.unwrap_or_default();
        bail!("GitHub asset upload failed ({}): {}", code, body);
    }

    info!("XVA uploaded successfully");
    Ok(())
}

// FIX #17: `type Result<T> = anyhow::Result<T>` that was at line 1002 has been
// removed entirely. `use anyhow::Result` at the top of the file already brings
// anyhow::Result into scope — the alias was redundant and confusing.
